use crate::{
    markers::ParallelSend, qjs, Ctx, Error, FromJs, IntoJs, JsLifetime, Object, Result, Value,
};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{
    ffi::c_void,
    fmt,
    mem::{self, size_of, MaybeUninit},
    ops::Deref,
    ptr::NonNull,
    result::Result as StdResult,
    slice,
};

use super::typed_array::TypedArrayItem;

/// JS_NewArrayBuffer uses max_len=0 for fixed-length buffers, and calls the
/// realloc callback with size=0 to free. Same sentinel, different meanings.
const FIXED_SIZE: qjs::size_t = 0;
const FREE: qjs::size_t = 0;

/// A contiguous byte region owned by `self` and usable as the backing store
/// of an [`ArrayBuffer`].
///
/// # Safety
///
/// * `as_ptr()` must return a pointer valid for reads of `len()` initialized bytes.
/// * The bytes must remain stable without unsynchronized external writers while
///   the engine can read them. Mutable constructors require additional exclusive
///   ownership obligations from their caller.
/// * The returned pointer must remain valid until `self` is dropped, including
///   across moves of `self`.
pub unsafe trait ArrayBufferSource {
    fn as_ptr(&self) -> *mut u8;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

macro_rules! impl_array_buffer_source {
    ($($t:ty),* $(,)?) => {
        $(
            unsafe impl ArrayBufferSource for $t {
                fn as_ptr(&self) -> *mut u8 {
                    <[u8]>::as_ptr(self) as *mut u8
                }
                fn len(&self) -> usize {
                    <[u8]>::len(self)
                }
            }
        )*
    };
}

impl_array_buffer_source!(Vec<u8>, alloc::boxed::Box<[u8]>, Arc<[u8]>, Arc<Vec<u8>>);

#[cfg(feature = "bytes")]
impl_array_buffer_source!(bytes::Bytes);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AsSliceError {
    BufferUsed,
    InvalidAlignment,
}

impl fmt::Display for AsSliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsSliceError::BufferUsed => write!(f, "Buffer was already used"),
            AsSliceError::InvalidAlignment => {
                write!(f, "Buffer had a different alignment than was requested")
            }
        }
    }
}

/// Rust representation of a JavaScript object of class ArrayBuffer.
///
#[derive(Debug, PartialEq, Clone, Eq, Hash)]
#[repr(transparent)]
pub struct ArrayBuffer<'js>(pub(crate) Object<'js>);

unsafe impl<'js> JsLifetime<'js> for ArrayBuffer<'js> {
    type Changed<'to> = ArrayBuffer<'to>;
}

impl<'js> ArrayBuffer<'js> {
    /// Create an array buffer by copying initialized scalar data into engine-owned memory.
    ///
    /// The source vector is dropped normally, including its unused capacity. Engine-owned
    /// storage allows JavaScript to resize or transfer the buffer without a Rust allocator callback.
    pub fn new<T: TypedArrayItem>(ctx: Ctx<'js>, src: impl Into<Vec<T>>) -> Result<Self> {
        Self::new_copy(ctx, src.into())
    }

    /// Create array buffer from slice
    ///
    /// ```compile_fail
    /// use rquickjs_core::{ArrayBuffer, Context, Runtime};
    /// let runtime = Runtime::new().unwrap();
    /// let context = Context::full(&runtime).unwrap();
    /// context.with(|ctx| {
    ///     // bool does not permit every bit pattern and cannot back a safe byte copy.
    ///     let _ = ArrayBuffer::new_copy(ctx, [true, false]);
    /// });
    /// ```
    pub fn new_copy<T: TypedArrayItem>(ctx: Ctx<'js>, src: impl AsRef<[T]>) -> Result<Self> {
        let src = src.as_ref();
        let ptr = src.as_ptr();
        let size = core::mem::size_of_val(src);

        Ok(Self(Object(unsafe {
            let val = qjs::JS_NewArrayBufferCopy(ctx.as_ptr(), ptr as _, size as _);
            ctx.handle_exception(val)?;
            Value::from_js_value(ctx.clone(), val)
        })))
    }

    /// Create an `ArrayBuffer` from a source that owns its backing bytes.
    ///
    /// The source is moved into the buffer; JS has exclusive mutable access
    /// until the buffer is collected, at which point the source is dropped.
    /// Using this with a shared-immutable source (`Arc<[u8]>`, `bytes::Bytes`,
    /// …) is unsound; use [`from_source_immutable`](Self::from_source_immutable) for those.
    ///
    /// # Safety
    ///
    /// The backing bytes must be writable and exclusively owned for the buffer's entire
    /// lifetime. No Rust references or external readers/writers may alias the bytes while
    /// JavaScript can mutate them. The pointer must have the native scalar alignment
    /// required by every typed-array or Atomics operation that JavaScript will perform.
    /// Transferring external storage can leak its ownership
    /// callback if allocation of the transfer target fails; callers must tolerate that leak.
    ///
    /// ```compile_fail
    /// use rquickjs_core::{ArrayBuffer, Context, Runtime};
    /// use std::sync::Arc;
    /// let runtime = Runtime::new().unwrap();
    /// let context = Context::full(&runtime).unwrap();
    /// let bytes: Arc<[u8]> = Arc::from([1, 2, 3]);
    /// context.with(|ctx| {
    ///     // Shared Rust bytes cannot silently become mutable JavaScript storage.
    ///     let _ = ArrayBuffer::from_source(ctx, bytes.clone());
    /// });
    /// ```
    pub unsafe fn from_source<S>(ctx: Ctx<'js>, src: S) -> Result<Self>
    where
        S: ArrayBufferSource + ParallelSend + 'static,
    {
        let ptr = src.as_ptr();
        let len = src.len();
        unsafe { Self::from_external(ctx, ptr, len, false, false, move || drop(src)) }
    }

    /// Create a `SharedArrayBuffer` from a source that owns its backing bytes.
    ///
    /// See [`from_source`](Self::from_source) for ownership semantics.
    ///
    /// # Safety
    ///
    /// The exclusive ownership obligations of `from_source` apply. The caller must also
    /// prevent concurrent writes or reads through external aliases or other JavaScript
    /// contexts whenever Rust accesses this shared backing store. The native-access
    /// alignment obligations also apply to typed-array and Atomics operations.
    pub unsafe fn from_source_shared<S>(ctx: Ctx<'js>, src: S) -> Result<Self>
    where
        S: ArrayBufferSource + ParallelSend + 'static,
    {
        let ptr = src.as_ptr();
        let len = src.len();
        unsafe { Self::from_external(ctx, ptr, len, true, false, move || drop(src)) }
    }

    /// Copy a possibly shared-immutable source into an engine-owned `ArrayBuffer`
    /// and mark the copy immutable.
    ///
    /// The native immutable flag does not cover every writer (notably Atomics).
    /// Copying ensures that JavaScript can never write into the original Rust source,
    /// including when Rust retains shared references such as `Arc<[u8]>` or `bytes::Bytes`.
    /// The source is dropped normally after construction, including on failure.
    pub fn from_source_immutable<S>(ctx: Ctx<'js>, src: S) -> Result<Self>
    where
        S: ArrayBufferSource + ParallelSend + 'static,
    {
        // ArrayBufferSource promises initialized, stable readable bytes for this copy.
        Ok(Self(Object(unsafe {
            let val = qjs::JS_NewArrayBufferCopy(ctx.as_ptr(), src.as_ptr(), src.len() as _);
            ctx.handle_exception(val)?;
            qjs::JS_SetImmutableArrayBuffer(val, true);
            Value::from_js_value(ctx, val)
        })))
    }

    /// Internal helper backing the `from_source*` constructors.
    ///
    /// `drop_fn` is invoked exactly once: when the buffer is garbage-collected,
    /// or synchronously if construction fails.
    ///
    /// # Safety
    ///
    /// `ptr` must point to `len` bytes of valid memory until `drop_fn` runs.
    unsafe fn from_external<F>(
        ctx: Ctx<'js>,
        ptr: *mut u8,
        len: usize,
        is_shared: bool,
        immutable: bool,
        drop_fn: F,
    ) -> Result<Self>
    where
        F: FnOnce() + ParallelSend + 'static,
    {
        extern "C" fn shim<F: FnOnce()>(
            _rt: *mut qjs::JSRuntime,
            opaque: *mut c_void,
            _ptr: *mut c_void,
            size: qjs::size_t,
        ) -> *mut c_void {
            if size != FREE {
                return core::ptr::null_mut();
            }
            unsafe {
                let boxed: Box<F> = Box::from_raw(opaque as *mut F);
                (*boxed)();
            }
            core::ptr::null_mut()
        }

        let opaque = Box::into_raw(Box::new(drop_fn)) as *mut c_void;

        Ok(Self(Object(unsafe {
            let val = qjs::JS_NewArrayBuffer(
                ctx.as_ptr(),
                ptr,
                len as _,
                FIXED_SIZE,
                Some(shim::<F>),
                opaque,
                is_shared,
            );
            if let Err(e) = ctx.handle_exception(val) {
                shim::<F>(
                    qjs::JS_GetRuntime(ctx.as_ptr()),
                    opaque,
                    ptr as *mut c_void,
                    FREE,
                );
                return Err(e);
            }
            if immutable {
                qjs::JS_SetImmutableArrayBuffer(val, true);
            }
            Value::from_js_value(ctx, val)
        })))
    }

    /// Get the length of the array buffer in bytes.
    pub fn len(&self) -> usize {
        Self::get_raw(&self.0).expect("Not an ArrayBuffer").len()
    }

    /// Returns whether an array buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the underlying bytes of the buffer,
    ///
    /// Returns `None` if the array is detached.
    ///
    /// # Safety
    ///
    /// The returned slice aliases the backing store. For the slice's entire lifetime,
    /// prevent JavaScript execution, detach, resize, and any external or other-thread
    /// writes to that store. This includes writers through SharedArrayBuffer aliases.
    /// Keep the buffer and its backing owner alive and do not create mutable aliases.
    pub unsafe fn as_bytes(&self) -> Option<&[u8]> {
        Some(self.as_raw()?.as_ref())
    }

    /// Returns a slice if the buffer underlying buffer is properly aligned for the type and the
    /// buffer is not detached.
    ///
    /// # Safety
    ///
    /// The returned slice aliases the backing store. For the slice's entire lifetime,
    /// prevent JavaScript execution, detach, resize, and any external or other-thread
    /// writes to that store. This includes writers through SharedArrayBuffer aliases.
    /// Keep the buffer and its backing owner alive and do not create mutable aliases.
    pub unsafe fn as_slice<T: TypedArrayItem>(&self) -> StdResult<&[T], AsSliceError> {
        let raw = Self::get_raw(&self.0).ok_or(AsSliceError::BufferUsed)?;
        if raw.cast::<u8>().align_offset(mem::align_of::<T>()) != 0 {
            return Err(AsSliceError::InvalidAlignment);
        }
        let len = raw.len() / size_of::<T>();
        Ok(slice::from_raw_parts(raw.as_ptr().cast(), len))
    }

    /// Detach array buffer
    pub fn detach(&mut self) {
        unsafe { qjs::JS_DetachArrayBuffer(self.0.ctx.as_ptr(), self.0.as_js_value()) }
    }

    /// Reference to value
    #[inline]
    pub fn as_value(&self) -> &Value<'js> {
        self.0.as_value()
    }

    /// Convert into value
    #[inline]
    pub fn into_value(self) -> Value<'js> {
        self.0.into_value()
    }

    /// Convert from value
    pub fn from_value(value: Value<'js>) -> Option<Self> {
        Self::from_object(Object::from_value(value).ok()?)
    }

    /// Reference as an object
    #[inline]
    pub fn as_object(&self) -> &Object<'js> {
        &self.0
    }

    /// Convert into an object
    #[inline]
    pub fn into_object(self) -> Object<'js> {
        self.0
    }

    /// Convert from an object
    pub fn from_object(object: Object<'js>) -> Option<Self> {
        if Self::get_raw(&object.0).is_some() {
            Some(Self(object))
        } else {
            None
        }
    }

    /// Returns a pointer to the underlying bytes of the buffer,
    ///
    /// The returned pointer is only guaranteed valid until the next time
    /// JavaScript runs: JS can write through it, detach the buffer, or, for a
    /// resizable buffer, reallocate the backing store and free this pointer.
    /// Treat the pointer as invalidated after any call back into the engine. External
    /// backing-store writers may also mutate it; dereferencing requires the same
    /// lifetime and aliasing guarantees as `as_bytes`.
    ///
    /// Returns None if the buffer was already detached.
    pub fn as_raw(&self) -> Option<NonNull<[u8]>> {
        Self::get_raw(self.as_value())
    }

    pub(crate) fn get_raw(val: &Value<'js>) -> Option<NonNull<[u8]>> {
        let ctx = val.ctx();
        let val = val.as_js_value();
        let mut size = MaybeUninit::<qjs::size_t>::uninit();
        let ptr = unsafe { qjs::JS_GetArrayBuffer(ctx.as_ptr(), size.as_mut_ptr(), val) };

        if let Some(ptr) = NonNull::new(ptr) {
            let len = unsafe { size.assume_init() }
                .try_into()
                .expect(qjs::SIZE_T_ERROR);
            Some(NonNull::slice_from_raw_parts(ptr, len))
        } else {
            None
        }
    }
}

impl<'js> Deref for ArrayBuffer<'js> {
    type Target = Object<'js>;

    fn deref(&self) -> &Self::Target {
        self.as_object()
    }
}

impl<'js> AsRef<Object<'js>> for ArrayBuffer<'js> {
    fn as_ref(&self) -> &Object<'js> {
        self.as_object()
    }
}

impl<'js> AsRef<Value<'js>> for ArrayBuffer<'js> {
    fn as_ref(&self) -> &Value<'js> {
        self.as_value()
    }
}

impl<'js> FromJs<'js> for ArrayBuffer<'js> {
    fn from_js(_: &Ctx<'js>, value: Value<'js>) -> Result<Self> {
        let ty_name = value.type_name();
        if let Some(v) = Self::from_value(value) {
            Ok(v)
        } else {
            Err(Error::new_from_js(ty_name, "ArrayBuffer"))
        }
    }
}

impl<'js> IntoJs<'js> for ArrayBuffer<'js> {
    fn into_js(self, _: &Ctx<'js>) -> Result<Value<'js>> {
        Ok(self.into_value())
    }
}

impl<'js> Object<'js> {
    /// Returns whether the object is an instance of [`ArrayBuffer`].
    pub fn is_array_buffer(&self) -> bool {
        ArrayBuffer::get_raw(&self.0).is_some()
    }

    /// Interpret as [`ArrayBuffer`]
    ///
    /// # Safety
    /// You should be sure that the object actually is the required type.
    pub unsafe fn ref_array_buffer(&self) -> &ArrayBuffer {
        mem::transmute(self)
    }

    /// Turn the object into an array buffer if the object is an instance of [`ArrayBuffer`].
    pub fn as_array_buffer(&self) -> Option<&ArrayBuffer> {
        self.is_array_buffer()
            .then_some(unsafe { self.ref_array_buffer() })
    }
}

#[cfg(test)]
mod test {
    use crate::*;
    use alloc::sync::Arc;

    #[test]
    fn from_javascript_i8() {
        test_with(|ctx| {
            let val: ArrayBuffer = ctx
                .eval(
                    r#"
                        new Int8Array([0, -5, 1, 11]).buffer
                    "#,
                )
                .unwrap();
            assert_eq!(val.len(), 4);
            assert_eq!(
                unsafe { val.as_slice() }.unwrap() as &[i8],
                &[0i8, -5, 1, 11]
            );
        });
    }

    #[test]
    fn into_javascript_i8() {
        test_with(|ctx| {
            let val = ArrayBuffer::new(ctx.clone(), [-1i8, 0, 22, 5]).unwrap();
            ctx.globals().set("a", val).unwrap();
            let res: i8 = ctx
                .eval(
                    r#"
                        let v = new Int8Array(a);
                        v.length != 4 ? 1 :
                        v[0] != -1 ? 2 :
                        v[1] != 0 ? 3 :
                        v[2] != 22 ? 4 :
                        v[3] != 5 ? 5 :
                        0
                    "#,
                )
                .unwrap();
            assert_eq!(res, 0);
        })
    }

    #[test]
    fn from_javascript_f32() {
        test_with(|ctx| {
            let val: ArrayBuffer = ctx
                .eval(
                    r#"
                        new Float32Array([0.5, -5.25, 123.125]).buffer
                    "#,
                )
                .unwrap();
            assert_eq!(val.len(), 12);
            assert_eq!(
                unsafe { val.as_slice() }.unwrap() as &[f32],
                &[0.5f32, -5.25, 123.125]
            );
        });
    }

    #[test]
    fn into_javascript_f32() {
        test_with(|ctx| {
            let val = ArrayBuffer::new(ctx.clone(), [-1.5f32, 0.0, 2.25]).unwrap();
            ctx.globals().set("a", val).unwrap();
            let res: i8 = ctx
                .eval(
                    r#"
                        let v = new Float32Array(a);
                        a.byteLength != 12 ? 1 :
                        v.length != 3 ? 2 :
                        v[0] != -1.5 ? 3 :
                        v[1] != 0 ? 4 :
                        v[2] != 2.25 ? 5 :
                        0
                    "#,
                )
                .unwrap();
            assert_eq!(res, 0);
        })
    }

    #[test]
    fn as_bytes() {
        test_with(|ctx| {
            let val: ArrayBuffer = ctx
                .eval(
                    r#"
                        new Uint32Array([0xCAFEDEAD,0xFEEDBEAD]).buffer
                    "#,
                )
                .unwrap();
            let mut res = [0; 8];
            let bytes_0 = 0xCAFEDEADu32.to_ne_bytes();
            res[..4].copy_from_slice(&bytes_0);
            let bytes_1 = 0xFEEDBEADu32.to_ne_bytes();
            res[4..].copy_from_slice(&bytes_1);

            assert_eq!(unsafe { val.as_bytes() }.unwrap(), &res)
        });
    }

    #[test]
    fn from_source_external_buffer() {
        use core::sync::atomic::{AtomicBool, Ordering};

        static DROPPED: AtomicBool = AtomicBool::new(false);

        struct Tracker(alloc::boxed::Box<[u8]>);
        unsafe impl ArrayBufferSource for Tracker {
            fn as_ptr(&self) -> *mut u8 {
                self.0.as_ptr()
            }
            fn len(&self) -> usize {
                self.0.len()
            }
        }
        impl Drop for Tracker {
            fn drop(&mut self) {
                DROPPED.store(true, Ordering::SeqCst);
            }
        }

        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let src = Tracker(alloc::vec![1u8, 2, 3, 4].into_boxed_slice());
            let ab = unsafe { ArrayBuffer::from_source(ctx.clone(), src) }.unwrap();
            assert_eq!(ab.len(), 4);
            assert_eq!(unsafe { ab.as_bytes() }.unwrap(), &[1, 2, 3, 4]);
        });
        rt.run_gc();
        assert!(DROPPED.load(Ordering::SeqCst));
    }

    #[test]
    fn from_source_error_invokes_drop_fn() {
        use core::sync::atomic::{AtomicBool, Ordering};

        static DROPPED: AtomicBool = AtomicBool::new(false);

        // A source that lies about its length to force construction failure,
        // and signals via `Drop` that the source was released exactly once.
        struct BadSource(#[allow(dead_code)] alloc::boxed::Box<[u8]>);
        unsafe impl ArrayBufferSource for BadSource {
            fn as_ptr(&self) -> *mut u8 {
                self.0.as_ptr()
            }
            fn len(&self) -> usize {
                i64::MAX as usize
            }
        }
        impl Drop for BadSource {
            fn drop(&mut self) {
                DROPPED.store(true, Ordering::SeqCst);
            }
        }

        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let src = BadSource(alloc::vec![1u8, 2, 3, 4].into_boxed_slice());
            let err = unsafe { ArrayBuffer::from_source(ctx.clone(), src) };
            assert!(err.is_err());
        });
        assert!(DROPPED.load(Ordering::SeqCst));
    }

    #[test]
    fn from_source_immutable_arc_slices() {
        struct ArcSlice {
            arc: Arc<Vec<u8>>,
            offset: usize,
            len: usize,
        }
        unsafe impl ArrayBufferSource for ArcSlice {
            fn as_ptr(&self) -> *mut u8 {
                unsafe { self.arc.as_ptr().add(self.offset) }
            }
            fn len(&self) -> usize {
                self.len
            }
        }

        let buf: Arc<Vec<u8>> = Arc::new((0u8..16).collect());
        let weak = Arc::downgrade(&buf);

        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let mk = |offset: usize, len: usize| -> ArrayBuffer<'_> {
                ArrayBuffer::from_source_immutable(
                    ctx.clone(),
                    ArcSlice {
                        arc: buf.clone(),
                        offset,
                        len,
                    },
                )
                .unwrap()
            };
            let full = mk(0, 16);
            let head = mk(0, 4);
            let tail = mk(12, 4);
            let middle = mk(4, 8);

            assert_eq!(
                unsafe { full.as_bytes() }.unwrap(),
                (0u8..16).collect::<Vec<_>>()
            );
            assert_eq!(unsafe { head.as_bytes() }.unwrap(), &[0, 1, 2, 3]);
            assert_eq!(unsafe { tail.as_bytes() }.unwrap(), &[12, 13, 14, 15]);
            assert_eq!(
                unsafe { middle.as_bytes() }.unwrap(),
                (4u8..12).collect::<Vec<_>>()
            );
            assert_eq!(
                Arc::strong_count(&buf),
                1,
                "copies must not retain source Arcs"
            );

            ctx.globals().set("buf", full).unwrap();

            let after: u8 = ctx
                .eval::<u8, _>(
                    r#"
                        const arr = new Uint8Array(buf);
                        arr[0] = 99;
                        arr[0];
                    "#,
                )
                .unwrap();
            assert_eq!(after, 0, "immutable ArrayBuffer must not accept writes");

            let writer = ctx.eval::<(), _>(
                r#"
                    "use strict";
                    new DataView(buf).setUint8(0, 99);
                "#,
            );
            assert!(
                writer.is_err(),
                "DataView write on immutable buffer must throw"
            );
        });

        drop(buf);
        rt.run_gc();
        drop(c);
        drop(rt);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn from_source_vec() {
        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let ab = unsafe { ArrayBuffer::from_source(ctx.clone(), alloc::vec![1u8, 2, 3, 4]) }
                .unwrap();
            assert_eq!(unsafe { ab.as_bytes() }.unwrap(), &[1, 2, 3, 4]);
        });
    }

    #[test]
    fn transfer_to_different_length_grows_vec_buffer() {
        test_with(|ctx| {
            let ab = ArrayBuffer::new(ctx.clone(), alloc::vec![1u8, 2, 3, 4]).unwrap();
            ctx.globals().set("buf", ab).unwrap();

            ctx.eval::<(), _>("globalThis.grown = buf.transfer(8);")
                .unwrap();

            // A detached buffer fails `FromJs`, so fetch it as a plain `Object`.
            let original: Object = ctx.globals().get("buf").unwrap();
            assert!(
                ArrayBuffer::from_object(original).is_none(),
                "source buffer must be detached by transfer"
            );

            let grown: ArrayBuffer = ctx.globals().get("grown").unwrap();
            assert_eq!(
                unsafe { grown.as_bytes() }.unwrap(),
                &[1, 2, 3, 4, 0, 0, 0, 0]
            );
        });
    }

    #[test]
    fn transfer_to_different_length_shrinks_vec_buffer() {
        test_with(|ctx| {
            let ab = ArrayBuffer::new(ctx.clone(), alloc::vec![1u8, 2, 3, 4]).unwrap();
            ctx.globals().set("buf", ab).unwrap();

            ctx.eval::<(), _>("globalThis.shrunk = buf.transfer(2);")
                .unwrap();

            let shrunk: ArrayBuffer = ctx.globals().get("shrunk").unwrap();
            assert_eq!(unsafe { shrunk.as_bytes() }.unwrap(), &[1, 2]);
        });
    }

    #[test]
    fn transfer_chain_grows_then_shrinks_vec_buffer() {
        test_with(|ctx| {
            let ab = ArrayBuffer::new(ctx.clone(), alloc::vec![1u8, 2, 3, 4]).unwrap();
            ctx.globals().set("buf", ab).unwrap();

            // Grow far enough to force a real move, then shrink, then grow again.
            ctx.eval::<(), _>(
                r#"
                    globalThis.mid = buf.transfer(64);
                    globalThis.small = mid.transfer(3);
                    globalThis.end = small.transfer(50);
                "#,
            )
            .unwrap();

            let end: ArrayBuffer = ctx.globals().get("end").unwrap();
            let bytes = unsafe { end.as_bytes() }.unwrap();
            assert_eq!(bytes.len(), 50);
            assert_eq!(&bytes[..3], &[1, 2, 3]);
            assert!(bytes[3..].iter().all(|&b| b == 0));
        });
    }

    #[test]
    fn transfer_to_different_length_preserves_external_buffer() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        struct Tracker {
            data: alloc::boxed::Box<[u8]>,
            drops: Arc<AtomicUsize>,
        }

        unsafe impl ArrayBufferSource for Tracker {
            fn as_ptr(&self) -> *mut u8 {
                self.data.as_ptr()
            }

            fn len(&self) -> usize {
                self.data.len()
            }
        }

        impl Drop for Tracker {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let src = Tracker {
                data: alloc::vec![1u8, 2, 3, 4].into_boxed_slice(),
                drops: drops.clone(),
            };
            let ab = unsafe { ArrayBuffer::from_source(ctx.clone(), src) }.unwrap();
            ctx.globals().set("buf", ab).unwrap();

            assert!(ctx.eval::<(), _>("buf.transfer(8)").is_err());
            drop(ctx.catch());
            assert_eq!(drops.load(Ordering::SeqCst), 0);

            let ab: ArrayBuffer = ctx.globals().get("buf").unwrap();
            assert_eq!(unsafe { ab.as_bytes() }.unwrap(), &[1, 2, 3, 4]);
        });

        drop(c);
        drop(rt);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn from_source_immutable_arc_is_copied_without_retaining_source() {
        let arc: Arc<[u8]> = Arc::from((0u8..8).collect::<Vec<_>>().into_boxed_slice());
        let weak = Arc::downgrade(&arc);
        assert_eq!(Arc::strong_count(&arc), 1);

        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();

        // Case 1: JS drops first while Rust keeps its Arc clone.
        c.with(|ctx| {
            let _ab = ArrayBuffer::from_source_immutable(ctx.clone(), arc.clone()).unwrap();
            assert_eq!(
                Arc::strong_count(&arc),
                1,
                "source clone released after copy"
            );
            // _ab owns only engine memory.
        });
        assert_eq!(
            Arc::strong_count(&arc),
            1,
            "engine memory must not retain an Arc clone"
        );
        assert!(
            weak.upgrade().is_some(),
            "allocation must stay alive while Rust still holds the Arc"
        );

        // Case 2: Rust drops first, JS keeps the buffer (stored in globals).
        c.with(|ctx| {
            let ab = ArrayBuffer::from_source_immutable(ctx.clone(), arc.clone()).unwrap();
            ctx.globals().set("buf", ab).unwrap();
            assert_eq!(Arc::strong_count(&arc), 1);
        });
        drop(arc);
        assert!(
            weak.upgrade().is_none(),
            "Rust source must be released independently of JS"
        );
        c.with(|ctx| {
            let ab: ArrayBuffer = ctx.globals().get("buf").unwrap();
            assert_eq!(unsafe { ab.as_bytes() }.unwrap(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        });

        // Drop the context: only the independent engine copy is freed.
        drop(c);
        drop(rt);
        assert!(
            weak.upgrade().is_none(),
            "allocation must be freed after both Rust and JS release their handles"
        );
    }

    #[cfg(feature = "bytes")]
    #[test]
    fn from_source_immutable_bytes() {
        let data: bytes::Bytes = (0u8..8).collect::<Vec<_>>().into();
        let rt = crate::Runtime::new().unwrap();
        let c = crate::Context::full(&rt).unwrap();
        c.with(|ctx| {
            let ab = ArrayBuffer::from_source_immutable(ctx.clone(), data.clone()).unwrap();
            assert_eq!(
                unsafe { ab.as_bytes() }.unwrap(),
                (0u8..8).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn spare_vector_capacity_never_becomes_buffer_data() {
        test_with(|ctx| {
            let mut source = alloc::vec::Vec::with_capacity(1024);
            source.extend_from_slice(&[10u32, 20, 30]);
            let buffer = ArrayBuffer::new(ctx.clone(), source).unwrap();
            assert_eq!(buffer.len(), 12);
            assert_eq!(unsafe { buffer.as_slice::<u32>() }.unwrap(), &[10, 20, 30]);
            ctx.globals().set("buffer", buffer).unwrap();
            let grown: ArrayBuffer = ctx.eval("buffer.transfer(24)").unwrap();
            assert_eq!(
                unsafe { grown.as_slice::<u32>() }.unwrap(),
                &[10, 20, 30, 0, 0, 0]
            );
            ctx.globals().set("grown", grown).unwrap();
            let shrunk: ArrayBuffer = ctx.eval("grown.transfer(4)").unwrap();
            assert_eq!(unsafe { shrunk.as_slice::<u32>() }.unwrap(), &[10]);
        });
    }

    #[test]
    fn copy_constructor_keeps_rust_source_immutable() {
        test_with(|ctx| {
            let source = [1u8, 2, 3];
            let buffer = ArrayBuffer::new_copy(ctx.clone(), &source).unwrap();
            ctx.globals().set("buffer", buffer).unwrap();
            ctx.eval::<(), _>("new Uint8Array(buffer)[0] = 99").unwrap();
            assert_eq!(source, [1, 2, 3]);
        });
    }
    #[test]
    fn immutable_source_copy_isolates_native_atomic_writers() {
        test_with(|ctx| {
            let source: Arc<[u8]> = Arc::from([1u8, 2, 3]);
            let buffer = ArrayBuffer::from_source_immutable(ctx.clone(), source.clone()).unwrap();
            // This assertion safely catches the original alias defect before any JS write.
            assert_ne!(
                buffer.as_raw().unwrap().cast::<u8>().as_ptr(),
                source.as_ptr() as *mut u8
            );
            ctx.globals().set("buffer", buffer).unwrap();
            ctx.eval::<(), _>("globalThis.view = new Uint8Array(buffer); Atomics.store(view, 0, 99); Atomics.add(view, 1, 4)").unwrap();
            assert_eq!(&*source, &[1, 2, 3]);
            let buffer: ArrayBuffer = ctx.globals().get("buffer").unwrap();
            assert_eq!(unsafe { buffer.as_bytes() }.unwrap(), &[99, 6, 3]);
        });
    }
}
