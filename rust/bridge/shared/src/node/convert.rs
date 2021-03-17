//
// Copyright 2021 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use neon::prelude::*;
use paste::paste;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::convert::{TryFrom, TryInto};
use std::hash::Hasher;
use std::ops::{Deref, RangeInclusive};
use std::slice;

use super::*;

/// Converts arguments from their JavaScript form to their Rust form.
///
/// `ArgTypeInfo` has two required methods: `borrow` and `load_from`. The use site looks like this:
///
/// ```no_run
/// # use libsignal_bridge::node::*;
/// # use neon::prelude::*;
/// # struct Foo;
/// # impl SimpleArgTypeInfo for Foo {
/// #     type ArgType = JsObject;
/// #     fn convert_from(cx: &mut FunctionContext, _: Handle<JsObject>) -> NeonResult<Self> {
/// #         Ok(Foo)
/// #     }
/// # }
/// # fn test<'a>(cx: &mut FunctionContext<'a>, js_arg: Handle<'a, JsObject>) -> NeonResult<()> {
/// let mut js_arg_borrowed = Foo::borrow(cx, js_arg)?;
/// let rust_arg = Foo::load_from(&mut js_arg_borrowed);
/// #     Ok(())
/// # }
/// ```
///
/// The `'context` lifetime allows for borrowed values to depend on the current JS stack frame;
/// that is, they can be assured that referenced objects will not be GC'd out from under them.
///
/// `ArgTypeInfo` is used to implement the `bridge_fn` macro, but can also be used outside it.
///
/// If the Rust type can be directly loaded from `ArgType` with no local storage needed,
/// implement [`SimpleArgTypeInfo`] instead.
pub trait ArgTypeInfo<'storage, 'context: 'storage>: Sized {
    /// The JavaScript form of the argument (e.g. `JsNumber`).
    type ArgType: neon::types::Value;
    /// Local storage for the argument (ideally borrowed rather than copied).
    type StoredType: 'storage;
    /// "Borrows" the data in `foreign`, usually to establish a local lifetime or owning type.
    fn borrow(
        cx: &mut FunctionContext<'context>,
        foreign: Handle<'context, Self::ArgType>,
    ) -> NeonResult<Self::StoredType>;
    /// Loads the Rust value from the data that's been `stored` by [`borrow()`](Self::borrow()).
    fn load_from(stored: &'storage mut Self::StoredType) -> Self;
}

/// Converts arguments from their JavaScript form and saves them for use in an `async` function.
///
/// `AsyncArgTypeInfo` works very similarly to `ArgTypeInfo`, but with the added restriction that
/// the stored type is `'static` so that it can be used in an `async` closure. Additionally, the
/// stored type implements `[neon::prelude::Finalize]` so that it can be cleaned up in a JavaScript
/// context. This allows storing persistent references to JavaScript objects.
///
/// Conceptually, the use site for `AsyncArgTypeInfo` looks like this:
///
/// ```no_run
/// # use libsignal_bridge::node::*;
/// # use neon::prelude::*;
/// # extern crate signal_neon_futures;
/// # struct Foo;
/// # impl SimpleArgTypeInfo for Foo {
/// #     type ArgType = JsObject;
/// #     fn convert_from(cx: &mut FunctionContext, _: Handle<JsObject>) -> NeonResult<Self> {
/// #         Ok(Foo)
/// #     }
/// # }
/// # fn test<'a>(mut cx: FunctionContext<'a>, js_arg: Handle<'a, JsObject>) -> NeonResult<()> {
/// // DO NOT COPY THIS CODE - DOES NOT HANDLE ERRORS CORRECTLY
/// let mut js_arg_stored = Foo::save_async_arg(&mut cx, js_arg)?;
/// let promise = signal_neon_futures::promise(&mut cx, async move {
///     let rust_arg = Foo::load_async_arg(&mut js_arg_stored);
///     // ...
///     signal_neon_futures::settle_promise(|cx| {
///         js_arg_stored.finalize(cx);
///         // ...
///         # Ok(cx.undefined())
///     })
/// })?;
/// #     Ok(())
/// # }
/// ```
///
/// The full implementation generated by `bridge_fn` will correctly finalize local storage when
/// there are errors as well. It is not recommended to use `AsyncArgTypeInfo` manually.
///
/// If the Rust type can be directly loaded from `ArgType` with no extra local storage needed,
/// implement [`SimpleArgTypeInfo`] instead.
pub trait AsyncArgTypeInfo<'storage>: Sized {
    /// The JavaScript form of the argument (e.g. `JsNumber`).
    type ArgType: neon::types::Value;
    /// Local storage for the argument that can outlive the current JavaScript context.
    type StoredType: 'static + Finalize;
    /// Saves the data in `foreign` so that it can be used in an `async` context.
    fn save_async_arg(
        cx: &mut FunctionContext,
        foreign: Handle<Self::ArgType>,
    ) -> NeonResult<Self::StoredType>;
    /// Loads the Rust value from the data that's been `stored` by
    /// [`save_async_arg()`](Self::save_async_arg()).
    fn load_async_arg(stored: &'storage mut Self::StoredType) -> Self;
}

/// A simpler interface for [`ArgTypeInfo`] and [`AsyncArgTypeInfo`] for when no separate local
/// storage is needed.
///
/// This trait is easier to use when writing Neon functions manually:
///
/// ```no_run
/// # use libsignal_bridge::node::*;
/// # use neon::prelude::*;
/// # struct Foo;
/// impl SimpleArgTypeInfo for Foo {
///     type ArgType = JsObject;
///     fn convert_from(cx: &mut FunctionContext, _: Handle<JsObject>) -> NeonResult<Self> {
///         // ...
///         # Ok(Foo)
///     }
/// }
///
/// # fn test<'a>(mut cx: FunctionContext<'a>, js_arg: Handle<'a, JsObject>) -> NeonResult<()> {
/// let rust_arg = Foo::convert_from(&mut cx, js_arg)?;
/// #     Ok(())
/// # }
/// ```
///
/// However, some types do need the full flexibility of `ArgTypeInfo` or `AsyncArgTypeInfo`.
pub trait SimpleArgTypeInfo: Sized + 'static {
    /// The JavaScript form of the argument (e.g. `JsNumber`).
    type ArgType: neon::types::Value;
    /// Converts the data in `foreign` to the Rust type.
    fn convert_from(cx: &mut FunctionContext, foreign: Handle<Self::ArgType>) -> NeonResult<Self>;
}

impl<'a, T> ArgTypeInfo<'a, 'a> for T
where
    T: SimpleArgTypeInfo,
{
    type ArgType = T::ArgType;
    type StoredType = Option<Self>;
    fn borrow(
        cx: &mut FunctionContext<'a>,
        foreign: Handle<'a, Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        Ok(Some(Self::convert_from(cx, foreign)?))
    }
    fn load_from(stored: &'a mut Self::StoredType) -> Self {
        stored.take().expect("should only be loaded once")
    }
}

impl<'a, T> AsyncArgTypeInfo<'a> for T
where
    T: SimpleArgTypeInfo,
{
    type ArgType = T::ArgType;
    type StoredType = super::DefaultFinalize<Option<Self>>;
    fn save_async_arg(
        cx: &mut FunctionContext,
        foreign: Handle<Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        Ok(super::DefaultFinalize(Some(Self::convert_from(
            cx, foreign,
        )?)))
    }
    fn load_async_arg(stored: &'a mut Self::StoredType) -> Self {
        stored.0.take().expect("should only be loaded once")
    }
}

/// Converts result values from their Rust form to their JavaScript form.
///
/// `ResultTypeInfo` is used to implement the `bridge_fn` macro, but can also be used outside it.
///
/// ```no_run
/// # use libsignal_bridge::node::*;
/// # use neon::prelude::*;
/// # struct Foo;
/// # impl<'a> ResultTypeInfo<'a> for Foo {
/// #     type ResultType = JsNumber;
/// #     fn convert_into(self, _cx: &mut impl Context<'a>) -> JsResult<'a, Self::ResultType> {
/// #         unimplemented!()
/// #     }
/// # }
/// # fn test(mut cx: FunctionContext) -> NeonResult<()> {
/// #     let rust_result = Foo;
/// let js_result = rust_result.convert_into(&mut cx)?;
/// #     Ok(())
/// # }
/// ```
///
/// Implementers should also see the `jni_result_type` macro in `convert.rs`.
pub trait ResultTypeInfo<'a>: Sized {
    /// The JavaScript form of the result (e.g. `JsNumber`).
    type ResultType: neon::types::Value;
    /// Converts the data in `self` to the JavaScript type, similar to `try_into()`.
    fn convert_into(self, cx: &mut impl Context<'a>) -> JsResult<'a, Self::ResultType>;
}

/// Returns `true` if `value` represents an integer within the given range.
fn can_convert_js_number_to_int(value: f64, valid_range: RangeInclusive<f64>) -> bool {
    value.is_finite() && value.fract() == 0.0 && valid_range.contains(&value)
}

// 2**53 - 1, the maximum "safe" integer representable in an f64.
// https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Number/MAX_SAFE_INTEGER
const MAX_SAFE_JS_INTEGER: f64 = 9007199254740991.0;

/// Converts non-negative numbers up to [`Number.MAX_SAFE_INTEGER`][].
///
/// [`Number.MAX_SAFE_INTEGER`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Number/MAX_SAFE_INTEGER
impl SimpleArgTypeInfo for u64 {
    type ArgType = JsNumber;
    fn convert_from(cx: &mut FunctionContext, foreign: Handle<Self::ArgType>) -> NeonResult<Self> {
        let value = foreign.value(cx);
        if !can_convert_js_number_to_int(value, 0.0..=MAX_SAFE_JS_INTEGER) {
            return cx.throw_range_error(format!("cannot convert {} to u64", value));
        }
        Ok(value as u64)
    }
}

impl SimpleArgTypeInfo for String {
    type ArgType = JsString;
    fn convert_from(cx: &mut FunctionContext, foreign: Handle<Self::ArgType>) -> NeonResult<Self> {
        Ok(foreign.value(cx))
    }
}

/// Converts `null` to `None`, passing through all other values.
impl<'storage, 'context: 'storage, T> ArgTypeInfo<'storage, 'context> for Option<T>
where
    T: ArgTypeInfo<'storage, 'context>,
{
    type ArgType = JsValue;
    type StoredType = Option<T::StoredType>;
    fn borrow(
        cx: &mut FunctionContext<'context>,
        foreign: Handle<'context, Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        if foreign.downcast::<JsNull, _>(cx).is_ok() {
            return Ok(None);
        }
        let non_optional_value = foreign.downcast_or_throw::<T::ArgType, _>(cx)?;
        T::borrow(cx, non_optional_value).map(Some)
    }
    fn load_from(stored: &'storage mut Self::StoredType) -> Self {
        stored.as_mut().map(T::load_from)
    }
}

/// A wrapper around `Option` that implements [`neon::prelude::Finalize`].
///
/// [Can be removed once we upgrade to the next Neon release.][pr]
///
/// [pr]: https://github.com/neon-bindings/neon/pull/680
pub struct FinalizableOption<T: Finalize>(Option<T>);

impl<T: Finalize> Finalize for FinalizableOption<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if let Some(value) = self.0 {
            value.finalize(cx)
        }
    }
}

/// Converts `null` to `None`, passing through all other values.
impl<'storage, T> AsyncArgTypeInfo<'storage> for Option<T>
where
    T: AsyncArgTypeInfo<'storage>,
{
    type ArgType = JsValue;
    type StoredType = FinalizableOption<T::StoredType>;
    fn save_async_arg(
        cx: &mut FunctionContext,
        foreign: Handle<Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        if foreign.downcast::<JsNull, _>(cx).is_ok() {
            return Ok(FinalizableOption(None));
        }
        let non_optional_value = foreign.downcast_or_throw::<T::ArgType, _>(cx)?;
        Ok(FinalizableOption(Some(T::save_async_arg(
            cx,
            non_optional_value,
        )?)))
    }
    fn load_async_arg(stored: &'storage mut Self::StoredType) -> Self {
        stored.0.as_mut().map(T::load_async_arg)
    }
}

/// Calculates a checksum to verify that a buffer wasn't mutated out from under us.
///
/// By default, this only checks the first 1024 bytes of the buffer, but it will check the entire
/// buffer if debug logging is enabled (`log::Level::Debug`).
fn calculate_checksum_for_immutable_buffer(buffer: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    const LIMIT: usize = 1024;
    if log::log_enabled!(log::Level::Debug) || buffer.len() < LIMIT {
        hasher.write(buffer);
    } else {
        hasher.write(&buffer[..LIMIT]);
    }
    hasher.finish()
}

/// A wrapper around `&[u8]` that also stores a checksum, to be validated on Drop.
pub struct AssumedImmutableBuffer<'a> {
    buffer: &'a [u8],
    hash: u64,
}

impl<'a> AssumedImmutableBuffer<'a> {
    /// Loads and checksums a slice from `handle`.
    ///
    /// [A JsBuffer owns its storage][napi], so it's safe to assume the buffer won't get
    /// deallocated. What's unsafe is assuming that no one else will modify the buffer while we
    /// have a reference to it, which is why we checksum it. (We can't stop the Rust compiler from
    /// potentially optimizing out that checksum, though.)
    ///
    /// [napi]: https://nodejs.org/api/n-api.html#n_api_napi_get_buffer_info
    fn new<'b>(cx: &mut impl Context<'b>, handle: Handle<'a, JsBuffer>) -> Self {
        let buffer = cx.borrow(&handle, |buf| {
            if buf.len() == 0 {
                &[]
            } else {
                unsafe { extend_lifetime::<'_, 'a, [u8]>(buf.as_slice()) }
            }
        });
        let hash = calculate_checksum_for_immutable_buffer(buffer);
        Self { buffer, hash }
    }
}

/// Logs an error (but does not panic) if the buffer's contents have changed.
impl Drop for AssumedImmutableBuffer<'_> {
    fn drop(&mut self) {
        if self.hash != calculate_checksum_for_immutable_buffer(self.buffer) {
            log::error!("buffer modified while in use");
        }
    }
}

/// Loads from a JsBuffer, assuming it won't be mutated while in use.
/// See [`AssumedImmutableBuffer`].
impl<'storage, 'context: 'storage> ArgTypeInfo<'storage, 'context> for &'storage [u8] {
    type ArgType = JsBuffer;
    type StoredType = AssumedImmutableBuffer<'context>;
    fn borrow(
        cx: &mut FunctionContext,
        foreign: Handle<'context, Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        Ok(AssumedImmutableBuffer::new(cx, foreign))
    }
    fn load_from(stored: &'storage mut Self::StoredType) -> Self {
        stored.buffer
    }
}

/// A wrapper around a persisted JavaScript buffer and a pointer/length pair.
///
/// Like [`AssumedImmutableBuffer`], `PersistentAssumedImmutableBuffer` also stores a checksum,
/// to be validated on Finalize.
///
/// A `PersistentAssumedImmutableBuffer` **cannot be dropped**; instead, it must be explicitly
/// finalized in a JavaScript context, as it contains a [`neon::handle::Root`].
pub struct PersistentAssumedImmutableBuffer {
    owner: Root<JsBuffer>,
    buffer_start: *const u8,
    buffer_len: usize,
    hash: u64,
}

impl PersistentAssumedImmutableBuffer {
    /// Establishes a GC root for `buffer`, then loads and checksums a slice from it.
    ///
    /// [A JsBuffer owns its storage][napi], so it's safe to assume the buffer won't get
    /// deallocated. What's unsafe is assuming that no one else will modify the buffer while we
    /// have a reference to it, which is why we checksum it. (We can't stop the Rust compiler from
    /// potentially optimizing out that checksum, though.)
    ///
    /// [napi]: https://nodejs.org/api/n-api.html#n_api_napi_get_buffer_info
    fn new<'a>(cx: &mut impl Context<'a>, buffer: Handle<JsBuffer>) -> Self {
        let owner = buffer.root(cx);
        let (buffer_start, buffer_len, hash) = cx.borrow(&buffer, |buf| {
            (
                if buf.len() == 0 {
                    std::ptr::null()
                } else {
                    buf.as_slice().as_ptr()
                },
                buf.len(),
                calculate_checksum_for_immutable_buffer(buf.as_slice()),
            )
        });
        Self {
            owner,
            buffer_start,
            buffer_len,
            hash,
        }
    }
}

impl Deref for PersistentAssumedImmutableBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        if self.buffer_start.is_null() {
            &[]
        } else {
            // See `new()` for the safety guarantee.
            unsafe { slice::from_raw_parts(self.buffer_start, self.buffer_len) }
        }
    }
}

// PersistentAssumedImmutableBuffer is not automatically Send because it contains a pointer.
// We're already assuming (and checking) that the contents of the buffer won't be modified
// while in use, and we know it won't be deallocated (see above).
unsafe impl Send for PersistentAssumedImmutableBuffer {}

/// Logs an error (but does not panic) if the buffer's contents have changed.
impl Finalize for PersistentAssumedImmutableBuffer {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if self.hash != calculate_checksum_for_immutable_buffer(&*self) {
            log::error!("buffer modified while in use");
        }
        self.owner.finalize(cx)
    }
}

/// Persists the JsBuffer, assuming it won't be mutated while in use.
/// See [`PersistentAssumedImmutableBuffer`].
impl<'a> AsyncArgTypeInfo<'a> for &'a [u8] {
    type ArgType = JsBuffer;
    type StoredType = PersistentAssumedImmutableBuffer;
    fn save_async_arg(
        cx: &mut FunctionContext,
        foreign: Handle<Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        Ok(PersistentAssumedImmutableBuffer::new(cx, foreign))
    }
    fn load_async_arg(stored: &'a mut Self::StoredType) -> Self {
        &*stored
    }
}

static_assertions::assert_type_eq_all!(libsignal_protocol::Context, Option<*mut std::ffi::c_void>);
impl<'a> AsyncArgTypeInfo<'a> for *mut std::ffi::c_void {
    type ArgType = JsNull;
    type StoredType = ();
    fn save_async_arg(
        _cx: &mut FunctionContext,
        _foreign: Handle<Self::ArgType>,
    ) -> NeonResult<Self::StoredType> {
        unreachable!() // only used as part of libsignal_protocol::Context
    }
    fn load_async_arg(_stored: &'a mut Self::StoredType) -> Self {
        unreachable!() // only used as part of libsignal_protocol::Context
    }
}

macro_rules! store {
    ($name:ident) => {
        paste! {
            impl<'a> AsyncArgTypeInfo<'a> for &'a mut dyn libsignal_protocol::$name {
                type ArgType = JsObject;
                type StoredType = [<Node $name>];
                fn save_async_arg(
                    cx: &mut FunctionContext,
                    foreign: Handle<Self::ArgType>,
                ) -> NeonResult<Self::StoredType> {
                    Ok(Self::StoredType::new(cx, foreign))
                }
                fn load_async_arg(stored: &'a mut Self::StoredType) -> Self {
                    stored
                }
            }
        }
    };
}

store!(IdentityKeyStore);
store!(PreKeyStore);
store!(SenderKeyStore);
store!(SessionStore);
store!(SignedPreKeyStore);

impl<'a> ResultTypeInfo<'a> for bool {
    type ResultType = JsBoolean;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        Ok(cx.boolean(self))
    }
}

/// Converts non-negative values up to [`Number.MAX_SAFE_INTEGER`][].
///
/// [`Number.MAX_SAFE_INTEGER`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Number/MAX_SAFE_INTEGER
impl<'a> ResultTypeInfo<'a> for u64 {
    type ResultType = JsNumber;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        let result = self as f64;
        if result > MAX_SAFE_JS_INTEGER {
            cx.throw_range_error(format!(
                "precision loss during conversion of {} to f64",
                self
            ))?;
        }
        Ok(cx.number(self as f64))
    }
}

impl<'a> ResultTypeInfo<'a> for String {
    type ResultType = JsString;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        self.deref().convert_into(cx)
    }
}

impl<'a> ResultTypeInfo<'a> for &str {
    type ResultType = JsString;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        Ok(cx.string(self))
    }
}

/// Converts `None` to `null`, passing through all other values.
impl<'a, T: ResultTypeInfo<'a>> ResultTypeInfo<'a> for Option<T> {
    type ResultType = JsValue;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        match self {
            Some(value) => Ok(value.convert_into(cx)?.upcast()),
            None => Ok(cx.null().upcast()),
        }
    }
}

impl<'a> ResultTypeInfo<'a> for Vec<u8> {
    type ResultType = JsBuffer;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        let bytes_len = match u32::try_from(self.len()) {
            Ok(l) => l,
            Err(_) => return cx.throw_error("Cannot return very large object to JS environment"),
        };

        let mut buffer = cx.buffer(bytes_len)?;
        cx.borrow_mut(&mut buffer, |raw_buffer| {
            raw_buffer.as_mut_slice().copy_from_slice(&self);
        });
        Ok(buffer)
    }
}

impl<'a, T: ResultTypeInfo<'a>> ResultTypeInfo<'a> for NeonResult<T> {
    type ResultType = T::ResultType;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        self?.convert_into(cx)
    }
}

impl<'a> ResultTypeInfo<'a> for () {
    type ResultType = JsUndefined;
    fn convert_into(self, cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        Ok(cx.undefined())
    }
}

impl<'a, T: Value> ResultTypeInfo<'a> for Handle<'a, T> {
    type ResultType = T;
    fn convert_into(self, _cx: &mut impl Context<'a>) -> NeonResult<Handle<'a, Self::ResultType>> {
        Ok(self)
    }
}

macro_rules! full_range_integer {
    ($typ:ty) => {
        #[doc = "Converts all valid integer values for the type."]
        impl SimpleArgTypeInfo for $typ {
            type ArgType = JsNumber;
            fn convert_from(
                cx: &mut FunctionContext,
                foreign: Handle<Self::ArgType>,
            ) -> NeonResult<Self> {
                let value = foreign.value(cx);
                if !can_convert_js_number_to_int(value, 0.0..=<$typ>::MAX.into()) {
                    return cx.throw_range_error(format!(
                        "cannot convert {} to {}",
                        value,
                        stringify!($typ),
                    ));
                }
                Ok(value as $typ)
            }
        }
        #[doc = "Converts all valid integer values for the type."]
        impl<'a> ResultTypeInfo<'a> for $typ {
            type ResultType = JsNumber;
            fn convert_into(
                self,
                cx: &mut impl Context<'a>,
            ) -> NeonResult<Handle<'a, Self::ResultType>> {
                Ok(cx.number(self as f64))
            }
        }
    };
}

full_range_integer!(u8);
full_range_integer!(u32);
full_range_integer!(i32);

/// Extremely unsafe function to extend the lifetime of a reference.
///
/// Only here so that we're not directly calling [`std::mem::transmute`], which is even more unsafe.
/// All call sites need to explain why extending the lifetime is safe.
pub(crate) unsafe fn extend_lifetime<'a, 'b: 'a, T: ?Sized>(some_ref: &'a T) -> &'b T {
    std::mem::transmute::<&'a T, &'b T>(some_ref)
}

/// The name of the property on JavaScript objects that wrap a boxed Rust value.
pub(crate) const NATIVE_HANDLE_PROPERTY: &str = "_nativeHandle";

/// Safely persists a boxed Rust value by treating its JavaScript wrapper as a GC root.
///
/// A `PersistentBoxedValue` **cannot be dropped**; instead, it must be explicitly
/// finalized in a JavaScript context, as it contains a [`neon::handle::Root`].
pub struct PersistentBoxedValue<T: Send + Sync + 'static> {
    owner: Root<JsObject>,
    value_ptr: *const T,
}

impl<T: Send + Sync + 'static> PersistentBoxedValue<T> {
    /// Persists `wrapper`, assuming it does in fact reference a boxed Rust value under the
    /// `_nativeHandle` property.
    pub(crate) fn new<'a>(
        cx: &mut impl Context<'a>,
        wrapper: Handle<JsObject>,
    ) -> NeonResult<Self> {
        let value_box: Handle<JsBox<T>> = wrapper
            .get(cx, NATIVE_HANDLE_PROPERTY)?
            .downcast_or_throw(cx)?;
        let value_ptr = &**value_box as *const T;
        // We must create the root after all failable operations.
        let owner = wrapper.root(cx);
        Ok(Self { owner, value_ptr })
    }
}

impl<T: Send + Sync + 'static> Deref for PersistentBoxedValue<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // We're unsafely assuming that `self.owner` still has a reference to the JsBox containing
        // the storage referenced by `self.value_ptr`.
        // N-API won't let us put a JsBox in a Root, so this indirection is necessary.
        unsafe { self.value_ptr.as_ref().expect("JsBox never contains NULL") }
    }
}

// PersistentBoxedValue is not automatically Send because it contains a pointer.
// We already know the contents of the value are only accessible to Rust, immutably,
// and we're promising it won't be deallocated (see above).
unsafe impl<T: Send + Sync + 'static> Send for PersistentBoxedValue<T> {}

impl<T: Send + Sync + 'static> Finalize for PersistentBoxedValue<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        self.owner.finalize(cx)
    }
}

/// Implementation of [`bridge_handle`](crate::support::bridge_handle) for Node.
macro_rules! node_bridge_handle {
    ( $typ:ty as false ) => {};
    ( $typ:ty as $node_name:ident ) => {
        impl<'storage, 'context: 'storage> node::ArgTypeInfo<'storage, 'context>
        for &'storage $typ {
            type ArgType = node::JsObject;
            type StoredType = node::Handle<'context, node::DefaultJsBox<$typ>>;
            fn borrow(
                cx: &mut node::FunctionContext<'context>,
                foreign: node::Handle<'context, Self::ArgType>,
            ) -> node::NeonResult<Self::StoredType> {
                node::Object::get(*foreign, cx, node::NATIVE_HANDLE_PROPERTY)?.downcast_or_throw(cx)
            }
            fn load_from(
                foreign: &'storage mut Self::StoredType,
            ) -> Self {
                &*foreign
            }
        }

        paste! {
            #[doc = "ts: interface " $typ " { readonly __type: unique symbol; }"]
            impl<'a> node::ResultTypeInfo<'a> for $typ {
                type ResultType = node::JsValue;
                fn convert_into(
                    self,
                    cx: &mut impl node::Context<'a>,
                ) -> node::NeonResult<node::Handle<'a, Self::ResultType>> {
                    node::return_boxed_object(cx, Ok(self))
                }
            }
        }

        impl<'storage> node::AsyncArgTypeInfo<'storage> for &'storage $typ {
            type ArgType = node::JsObject;
            type StoredType = node::PersistentBoxedValue<node::DefaultFinalize<$typ>>;
            fn save_async_arg(
                cx: &mut node::FunctionContext,
                foreign: node::Handle<Self::ArgType>,
            ) -> node::NeonResult<Self::StoredType> {
                node::PersistentBoxedValue::new(cx, foreign)
            }
            fn load_async_arg(
                stored: &'storage mut Self::StoredType,
            ) -> Self {
                &*stored
            }
        }
    };
    ( $typ:ty as $node_name:ident, mut = true ) => {
        impl<'storage, 'context: 'storage> node::ArgTypeInfo<'storage, 'context>
            for &'storage $typ
        {
            type ArgType = node::JsObject;
            type StoredType = (
                node::Handle<'context, node::DefaultJsBox<std::cell::RefCell<$typ>>>,
                std::cell::Ref<'context, $typ>,
            );
            fn borrow(
                cx: &mut node::FunctionContext<'context>,
                foreign: node::Handle<'context, Self::ArgType>,
            ) -> node::NeonResult<Self::StoredType> {
                let boxed_value: node::Handle<'context, node::DefaultJsBox<std::cell::RefCell<$typ>>> =
                    node::Object::get(*foreign, cx, node::NATIVE_HANDLE_PROPERTY)?.downcast_or_throw(cx)?;
                let cell: &std::cell::RefCell<_> = &***boxed_value;
                // FIXME: Workaround for https://github.com/neon-bindings/neon/issues/678
                // The lifetime of the boxed RefCell is necessarily longer than the lifetime of any handles referring to it, i.e. longer than 'context.
                // However, Deref'ing a Handle can only give us a Ref whose lifetime matches a *particular* handle.
                // Therefore, we unsafely (in the compiler sense) extend the lifetime to be the lifetime of the context, as given by the Handle.
                // (We also know the RefCell can't move because we can't know how many JS references there are referring to the JsBox.)
                let cell_with_extended_lifetime: &'context std::cell::RefCell<_> = unsafe {
                    node::extend_lifetime(cell)
                };
                Ok((boxed_value, cell_with_extended_lifetime.borrow()))
            }
            fn load_from(
                stored: &'storage mut Self::StoredType,
            ) -> Self {
                &*stored.1
            }
        }

        impl<'storage, 'context: 'storage> node::ArgTypeInfo<'storage, 'context>
            for &'storage mut $typ
        {
            type ArgType = node::JsObject;
            type StoredType = (
                node::Handle<'context, node::DefaultJsBox<std::cell::RefCell<$typ>>>,
                std::cell::RefMut<'context, $typ>,
            );
            fn borrow(
                cx: &mut node::FunctionContext<'context>,
                foreign: node::Handle<'context, Self::ArgType>,
            ) -> node::NeonResult<Self::StoredType> {
                let boxed_value: node::Handle<'context, node::DefaultJsBox<std::cell::RefCell<$typ>>> =
                    node::Object::get(*foreign, cx, node::NATIVE_HANDLE_PROPERTY)?.downcast_or_throw(cx)?;
                let cell: &std::cell::RefCell<_> = &***boxed_value;
                // See above.
                let cell_with_extended_lifetime: &'context std::cell::RefCell<_> = unsafe {
                    node::extend_lifetime(cell)
                };
                Ok((boxed_value, cell_with_extended_lifetime.borrow_mut()))
            }
            fn load_from(
                stored: &'storage mut Self::StoredType,
            ) -> Self {
                &mut *stored.1
            }
        }

        paste! {
            #[doc = "ts: interface " $typ " { readonly __type: unique symbol; }"]
            impl<'a> node::ResultTypeInfo<'a> for $typ {
                type ResultType = node::JsValue;
                fn convert_into(
                    self,
                    cx: &mut impl node::Context<'a>,
                ) -> node::NeonResult<node::Handle<'a, Self::ResultType>> {
                    node::return_boxed_object(cx, Ok(std::cell::RefCell::new(self)))
                }
            }
        }
    };
    ( $typ:ty $(, mut = $_:tt)?) => {
        paste! {
            node_bridge_handle!($typ as $typ $(, mut = $_)?);
        }
    };
}

impl<'a> crate::support::Env for &'_ mut FunctionContext<'a> {
    type Buffer = JsResult<'a, JsBuffer>;
    fn buffer<'b, T: Into<Cow<'b, [u8]>>>(self, input: T) -> Self::Buffer {
        let input = input.into();
        let len: u32 = input
            .len()
            .try_into()
            .or_else(|_| self.throw_error("buffer too large to return to JavaScript"))?;
        let mut result = Context::buffer(self, len)?;
        self.borrow_mut(&mut result, |buf| {
            buf.as_mut_slice().copy_from_slice(input.as_ref())
        });
        Ok(result)
    }
}

/// A dummy type used to implement [`crate::support::Env`] for `async` `bridge_fn`s.
pub(crate) struct AsyncEnv;

impl crate::support::Env for AsyncEnv {
    // FIXME: Can we avoid this copy?
    type Buffer = Vec<u8>;
    fn buffer<'b, T: Into<Cow<'b, [u8]>>>(self, input: T) -> Self::Buffer {
        input.into().into_owned()
    }
}
