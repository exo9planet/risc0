// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{cell::RefCell, fmt::Debug, marker::PhantomData, rc::Rc, sync::OnceLock};

use cust::{
    device::DeviceAttribute,
    function::{BlockSize, GridSize},
    memory::{DeviceCopy, DevicePointer, GpuBuffer},
    prelude::*,
};
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use risc0_core::field::{
    baby_bear::{BabyBear, BabyBearElem, BabyBearExtElem},
    ExtElem, RootsOfUnity,
};
use risc0_sys::cuda::*;

use super::{tracker, Buffer, Hal};
use crate::{
    core::{
        digest::Digest,
        hash::{
            poseidon::{self, PoseidonHashSuite},
            poseidon2::{self, Poseidon2HashSuite},
            sha::Sha256HashSuite,
            HashSuite,
        },
        log2_ceil,
    },
    FRI_FOLD,
};

const KERNELS_FATBIN: &[u8] = include_bytes!(env!("ZKP_CUDA_PATH"));

fn context() -> &'static Context {
    static ONCE: OnceLock<Context> = OnceLock::new();
    ONCE.get_or_init(|| {
        let device = Device::get_device(0).unwrap();
        let context = Context::new(device).unwrap();
        context.set_flags(ContextFlags::SCHED_AUTO).unwrap();
        context
    })
}

// The GPU becomes unstable as the number of concurrent provers grow.
fn singleton() -> &'static ReentrantMutex<()> {
    static ONCE: OnceLock<ReentrantMutex<()>> = OnceLock::new();
    ONCE.get_or_init(|| ReentrantMutex::new(()))
}

#[derive(Clone, Copy)]
pub struct DeviceExtElem(pub BabyBearExtElem);

unsafe impl DeviceCopy for DeviceExtElem {}

pub trait CudaHash {
    /// Create a hash implementation
    fn new(hal: &CudaHal<Self>) -> Self;

    /// Run the hash_fold function
    fn hash_fold(&self, hal: &CudaHal<Self>, io: &BufferImpl<Digest>, output_size: usize);

    /// Run the hash_rows function
    fn hash_rows(
        &self,
        hal: &CudaHal<Self>,
        output: &BufferImpl<Digest>,
        matrix: &BufferImpl<BabyBearElem>,
    );

    /// Return the HashSuite
    fn get_hash_suite(&self) -> &HashSuite<BabyBear>;
}

pub struct CudaHashSha256 {
    suite: HashSuite<BabyBear>,
}

impl CudaHash for CudaHashSha256 {
    fn new(_hal: &CudaHal<Self>) -> Self {
        CudaHashSha256 {
            suite: Sha256HashSuite::new_suite(),
        }
    }

    fn hash_fold(&self, hal: &CudaHal<Self>, io: &BufferImpl<Digest>, output_size: usize) {
        let kernel = hal.module.get_function("sha_fold").unwrap();
        let params = hal.compute_simple_params(output_size);
        unsafe {
            // DevicePointers require that the underlying type of the pointer implements the
            // DeviceCopy trait. core::Digest does not implement this trait.
            // TODO: refactor data types to allow safer copying.
            // Here, we perform pointer arithmetic on the underlying device_pointer of type
            // u8.
            // TODO: modify type hierarchy to fit Rustacuda's memory model
            // to allow for more type safe pointer arithmetic
            let input = io.as_device_ptr_with_offset(2 * output_size);
            let output = io.as_device_ptr_with_offset(output_size);
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output,
                input,
                output_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn hash_rows(
        &self,
        hal: &CudaHal<Self>,
        output: &BufferImpl<Digest>,
        matrix: &BufferImpl<BabyBearElem>,
    ) {
        let row_size = output.size();
        let col_size = matrix.size() / output.size();
        assert_eq!(matrix.size(), col_size * row_size);

        let kernel = hal.module.get_function("sha_rows").unwrap();
        let params = hal.compute_simple_params(row_size);
        unsafe {
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                matrix.as_device_ptr(),
                row_size,
                col_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn get_hash_suite(&self) -> &HashSuite<BabyBear> {
        &self.suite
    }
}

pub struct CudaHashPoseidon {
    suite: HashSuite<BabyBear>,
    round_constants: BufferImpl<BabyBearElem>,
    mds: BufferImpl<BabyBearElem>,
    partial_comp_matrix: BufferImpl<BabyBearElem>,
    partial_comp_offset: BufferImpl<BabyBearElem>,
}

impl CudaHash for CudaHashPoseidon {
    fn new(hal: &CudaHal<Self>) -> Self {
        let round_constants =
            hal.copy_from_elem("round_constants", poseidon::consts::ROUND_CONSTANTS);
        let mds = hal.copy_from_elem("mds", poseidon::consts::MDS);
        let partial_comp_matrix =
            hal.copy_from_elem("partial_comp_matrix", poseidon::consts::PARTIAL_COMP_MATRIX);
        let partial_comp_offset =
            hal.copy_from_elem("partial_comp_offset", poseidon::consts::PARTIAL_COMP_OFFSET);
        CudaHashPoseidon {
            suite: PoseidonHashSuite::new_suite(),
            round_constants,
            mds,
            partial_comp_matrix,
            partial_comp_offset,
        }
    }

    fn hash_fold(&self, hal: &CudaHal<Self>, io: &BufferImpl<Digest>, output_size: usize) {
        let kernel = hal.module.get_function("poseidon_fold").unwrap();
        let params = hal.compute_simple_params(output_size);
        unsafe {
            // DevicePointers require that the underlying type of the pointer implements the
            // DeviceCopy trait. core::Digest does not implement this trait.
            // TODO: refactor data types to allow safer copying.
            // Here, we perform pointer arithmetic on the underlying device_pointer of type
            // u8.
            // TODO: modify type hierarchy to fit Rustacuda's memory model
            // to allow for more type safe pointer arithmetic
            let input = io.as_device_ptr_with_offset(2 * output_size);
            let output = io.as_device_ptr_with_offset(output_size);
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                self.round_constants.as_device_ptr(),
                self.mds.as_device_ptr(),
                self.partial_comp_matrix.as_device_ptr(),
                self.partial_comp_offset.as_device_ptr(),
                output,
                input,
                output_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn hash_rows(
        &self,
        hal: &CudaHal<Self>,
        output: &BufferImpl<Digest>,
        matrix: &BufferImpl<BabyBearElem>,
    ) {
        let row_size = output.size();
        let col_size = matrix.size() / output.size();
        assert_eq!(matrix.size(), col_size * row_size);

        let kernel = hal.module.get_function("poseidon_rows").unwrap();
        let params = hal.compute_simple_params(row_size);
        unsafe {
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                self.round_constants.as_device_ptr(),
                self.mds.as_device_ptr(),
                self.partial_comp_matrix.as_device_ptr(),
                self.partial_comp_offset.as_device_ptr(),
                output.as_device_ptr(),
                matrix.as_device_ptr(),
                row_size,
                col_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn get_hash_suite(&self) -> &HashSuite<BabyBear> {
        &self.suite
    }
}

pub struct CudaHashPoseidon2 {
    suite: HashSuite<BabyBear>,
    round_constants: BufferImpl<BabyBearElem>,
    m_int_diag: BufferImpl<BabyBearElem>,
}

impl CudaHash for CudaHashPoseidon2 {
    fn new(hal: &CudaHal<Self>) -> Self {
        let round_constants =
            hal.copy_from_elem("round_constants", poseidon2::consts::ROUND_CONSTANTS);
        let m_int_diag = hal.copy_from_elem("m_int_diag", poseidon2::consts::M_INT_DIAG_HZN);
        CudaHashPoseidon2 {
            suite: Poseidon2HashSuite::new_suite(),
            round_constants,
            m_int_diag,
        }
    }

    fn hash_fold(&self, hal: &CudaHal<Self>, io: &BufferImpl<Digest>, output_size: usize) {
        let kernel = hal.module.get_function("poseidon2_fold").unwrap();
        let params = hal.compute_simple_params(output_size);
        unsafe {
            // DevicePointers require that the underlying type of the pointer implements the
            // DeviceCopy trait. core::Digest does not implement this trait.
            // TODO: refactor data types to allow safer copying.
            // Here, we perform pointer arithmetic on the underlying device_pointer of type
            // u8.
            // TODO: modify type hierarchy to fit Rustacuda's memory model
            // to allow for more type safe pointer arithmetic
            let input = io.as_device_ptr_with_offset(2 * output_size);
            let output = io.as_device_ptr_with_offset(output_size);
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                self.round_constants.as_device_ptr(),
                self.m_int_diag.as_device_ptr(),
                output,
                input,
                output_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn hash_rows(
        &self,
        hal: &CudaHal<Self>,
        output: &BufferImpl<Digest>,
        matrix: &BufferImpl<BabyBearElem>,
    ) {
        let row_size = output.size();
        let col_size = matrix.size() / output.size();
        assert_eq!(matrix.size(), col_size * row_size);

        let kernel = hal.module.get_function("poseidon2_rows").unwrap();
        let params = hal.compute_simple_params(row_size);
        unsafe {
            let stream = &hal.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                self.round_constants.as_device_ptr(),
                self.m_int_diag.as_device_ptr(),
                output.as_device_ptr(),
                matrix.as_device_ptr(),
                row_size,
                col_size
            ))
            .unwrap();
        }
        hal.stream.synchronize().unwrap();
    }

    fn get_hash_suite(&self) -> &HashSuite<BabyBear> {
        &self.suite
    }
}

pub struct CudaHal<Hash: CudaHash + ?Sized> {
    pub max_threads: u32,
    pub module: Module,
    hash: Option<Box<Hash>>,
    _context: Context,
    _lock: ReentrantMutexGuard<'static, ()>,
    pub stream: Stream,
}

pub type CudaHalSha256 = CudaHal<CudaHashSha256>;
pub type CudaHalPoseidon = CudaHal<CudaHashPoseidon>;
pub type CudaHalPoseidon2 = CudaHal<CudaHashPoseidon2>;

struct RawBuffer {
    name: &'static str,
    buf: DeviceBuffer<u8>,
}

impl RawBuffer {
    pub fn new(name: &'static str, size: usize) -> Self {
        tracing::trace!("alloc: {size} bytes, {name}");
        tracker().lock().unwrap().alloc(size);
        Self {
            name,
            buf: unsafe { DeviceBuffer::uninitialized(size).unwrap() },
        }
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        tracing::trace!("free: {} bytes, {}", self.buf.len(), self.name);
        tracker().lock().unwrap().free(self.buf.len());
    }
}

#[derive(Clone)]
pub struct BufferImpl<T> {
    buffer: Rc<RefCell<RawBuffer>>,
    size: usize,
    offset: usize,
    marker: PhantomData<T>,
}

#[inline]
fn unchecked_cast<A, B>(a: &[A]) -> &[B] {
    let new_len = std::mem::size_of_val(a) / std::mem::size_of::<B>();
    unsafe { std::slice::from_raw_parts(a.as_ptr() as *const B, new_len) }
}

#[inline]
fn unchecked_cast_mut<A, B>(a: &mut [A]) -> &mut [B] {
    let new_len = std::mem::size_of_val(a) / std::mem::size_of::<B>();
    unsafe { std::slice::from_raw_parts_mut(a.as_mut_ptr() as *mut B, new_len) }
}

impl<T> BufferImpl<T> {
    fn new(name: &'static str, size: usize) -> Self {
        let bytes_len = std::mem::size_of::<T>() * size;
        assert!(bytes_len > 0);
        BufferImpl {
            buffer: Rc::new(RefCell::new(RawBuffer::new(name, bytes_len))),
            size,
            offset: 0,
            marker: PhantomData,
        }
    }

    pub fn copy_from(name: &'static str, slice: &[T]) -> Self {
        // nvtx::range_push!("copy_from");
        let bytes_len = std::mem::size_of_val(slice);
        assert!(bytes_len > 0);
        let mut buffer = RawBuffer::new(name, bytes_len);
        let bytes = unchecked_cast(slice);
        buffer.buf.copy_from(bytes).unwrap();
        // nvtx::range_pop!();

        BufferImpl {
            buffer: Rc::new(RefCell::new(buffer)),
            size: slice.len(),
            offset: 0,
            marker: PhantomData,
        }
    }

    pub fn as_device_ptr(&self) -> DevicePointer<u8> {
        let ptr = self.buffer.borrow_mut().buf.as_device_ptr();
        let offset = self.offset * std::mem::size_of::<T>();
        unsafe { ptr.offset(offset.try_into().unwrap()) }
    }

    pub fn as_device_ptr_with_offset(&self, offset: usize) -> DevicePointer<u8> {
        let ptr = self.buffer.borrow_mut().buf.as_device_ptr();
        let offset = (self.offset + offset) * std::mem::size_of::<T>();
        unsafe { ptr.offset(offset.try_into().unwrap()) }
    }
}

impl<T: Clone> Buffer<T> for BufferImpl<T> {
    fn name(&self) -> &'static str {
        self.buffer.borrow().name
    }

    fn size(&self) -> usize {
        self.size
    }

    fn slice(&self, offset: usize, size: usize) -> BufferImpl<T> {
        assert!(offset + size <= self.size());
        BufferImpl {
            buffer: self.buffer.clone(),
            size,
            offset: self.offset + offset,
            marker: PhantomData,
        }
    }

    fn get_at(&self, idx: usize) -> T {
        let item_size = std::mem::size_of::<T>();
        let buf = self.buffer.borrow_mut();
        let offset = (self.offset + idx) * item_size;
        let ptr = unsafe { buf.buf.as_device_ptr().offset(offset as isize) };
        let device_slice = unsafe { DeviceSlice::from_raw_parts(ptr, item_size) };
        let host_buf = device_slice.as_host_vec().unwrap();
        let slice: &[T] = unchecked_cast(&host_buf);
        let item = slice[0].clone();
        item
    }

    fn view<F: FnOnce(&[T])>(&self, f: F) {
        nvtx::range_push!("view");
        let item_size = std::mem::size_of::<T>();
        let buf = self.buffer.borrow_mut();
        let offset = self.offset * item_size;
        let len = self.size * item_size;
        let ptr = unsafe { buf.buf.as_device_ptr().offset(offset as isize) };
        let device_slice = unsafe { DeviceSlice::from_raw_parts(ptr, len) };
        let host_buf = device_slice.as_host_vec().unwrap();
        let slice = unchecked_cast(&host_buf);
        f(slice);
        nvtx::range_pop!();
    }

    fn view_mut<F: FnOnce(&mut [T])>(&self, f: F) {
        nvtx::range_push!("view_mut");
        let mut buf = self.buffer.borrow_mut();
        let mut host_buf = buf.buf.as_host_vec().unwrap();
        let slice = unchecked_cast_mut(&mut host_buf);
        f(&mut slice[self.offset..]);
        buf.buf.copy_from(&host_buf).unwrap();
        nvtx::range_pop!();
    }
}

impl<CH: CudaHash> CudaHal<CH> {
    #[tracing::instrument(name = "CudaHal::new", skip_all)]
    pub fn new() -> Self {
        let _lock = singleton().lock();

        let err = unsafe { sppark_init() };
        if err.code != 0 {
            panic!("Failure during sppark_init: {err}");
        }

        cust::init(CudaFlags::empty()).unwrap();
        let device = Device::get_device(0).unwrap();
        let max_threads = device
            .get_attribute(DeviceAttribute::MaxThreadsPerBlock)
            .unwrap();
        let _context = context().clone();
        let module = Module::from_fatbin(KERNELS_FATBIN, &[]).unwrap();
        let stream = Stream::new(StreamFlags::DEFAULT, None).unwrap();
        let mut hal = Self {
            max_threads: max_threads as u32,
            module,
            _context,
            hash: None,
            _lock,
            stream,
        };
        let hash = Box::new(CH::new(&hal));
        hal.hash = Some(hash);
        hal
    }

    pub fn compute_simple_params(&self, count: usize) -> (GridSize, BlockSize) {
        let count: u32 = count.try_into().unwrap();
        let block = self.max_threads / 4;
        let grid = div_ceil(count, block);
        (GridSize::x(grid), BlockSize::x(block))
    }

    pub fn compute_launch_params(
        &self,
        n_bits: u32,
        s_bits: u32,
        c_size: u32,
    ) -> (GridSize, BlockSize) {
        let s_size = 1 << (s_bits - 1);
        let g_size = 1 << (n_bits - s_bits);

        let mut grid = GridSize::xyz(1, 1, 1);
        let mut block = BlockSize::xyz(1, 1, 1);

        let mut threads = 128;
        // First thread over S
        block.x = threads.min(s_size);
        threads /= block.x;
        // Next thread over G
        block.y = threads.min(g_size);
        // Don't bother threading over C
        let mut grids = 32;
        // First grid over S
        grid.x = grids.min(s_size / block.x);
        grids /= grid.x;
        // Next grid over G
        grid.y = grids.min(g_size / block.y);
        grids /= grid.y;
        // Next grid over C
        grid.z = grids.min(c_size);
        (grid, block)
    }
}

#[allow(unused_variables)]
impl<CH: CudaHash> Hal for CudaHal<CH> {
    type Field = BabyBear;
    type Elem = BabyBearElem;
    type ExtElem = BabyBearExtElem;
    type Buffer<T: Clone + Debug + PartialEq> = BufferImpl<T>;

    fn alloc_elem(&self, name: &'static str, size: usize) -> Self::Buffer<Self::Elem> {
        BufferImpl::new(name, size)
    }

    fn copy_from_elem(&self, name: &'static str, slice: &[Self::Elem]) -> Self::Buffer<Self::Elem> {
        BufferImpl::copy_from(name, slice)
    }

    fn alloc_extelem(&self, name: &'static str, size: usize) -> Self::Buffer<Self::ExtElem> {
        BufferImpl::new(name, size)
    }

    fn copy_from_extelem(
        &self,
        name: &'static str,
        slice: &[Self::ExtElem],
    ) -> Self::Buffer<Self::ExtElem> {
        BufferImpl::copy_from(name, slice)
    }

    fn alloc_digest(&self, name: &'static str, size: usize) -> Self::Buffer<Digest> {
        BufferImpl::new(name, size)
    }

    fn copy_from_digest(&self, name: &'static str, slice: &[Digest]) -> Self::Buffer<Digest> {
        BufferImpl::copy_from(name, slice)
    }

    fn alloc_u32(&self, name: &'static str, size: usize) -> Self::Buffer<u32> {
        BufferImpl::new(name, size)
    }

    fn copy_from_u32(&self, name: &'static str, slice: &[u32]) -> Self::Buffer<u32> {
        BufferImpl::copy_from(name, slice)
    }

    #[tracing::instrument(skip_all)]
    fn batch_expand_into_evaluate_ntt(
        &self,
        output: &Self::Buffer<Self::Elem>,
        input: &Self::Buffer<Self::Elem>,
        poly_count: usize,
        expand_bits: usize,
    ) {
        // batch_expand
        {
            let out_size = output.size() / poly_count;
            let in_size = input.size() / poly_count;
            let expand_bits = log2_ceil(out_size / in_size);
            assert_eq!(output.size(), out_size * poly_count);
            assert_eq!(input.size(), in_size * poly_count);
            assert_eq!(out_size, in_size * (1 << expand_bits));
            let in_bits = log2_ceil(in_size);
            let err = unsafe {
                batch_expand(
                    output.as_device_ptr(),
                    input.as_device_ptr(),
                    in_bits.try_into().unwrap(),
                    expand_bits.try_into().unwrap(),
                    poly_count.try_into().unwrap(),
                )
            };
            if err.code != 0 {
                panic!("Failure during batch_expand: {err}");
            }
        }

        // batch_evaluate_ntt
        {
            let row_size = output.size() / poly_count;
            assert_eq!(row_size * poly_count, output.size());
            let n_bits = log2_ceil(row_size);
            assert_eq!(row_size, 1 << n_bits);
            assert!(n_bits >= expand_bits);
            assert!(n_bits < Self::Elem::MAX_ROU_PO2);

            let err = unsafe {
                batch_NTT(
                    output.as_device_ptr(),
                    n_bits.try_into().unwrap(),
                    poly_count.try_into().unwrap(),
                )
            };
            if err.code != 0 {
                panic!("Failure during batch_evaluate_ntt: {err}");
            }
        }
    }

    fn batch_interpolate_ntt(&self, io: &Self::Buffer<Self::Elem>, count: usize) {
        let row_size = io.size() / count;
        assert_eq!(row_size * count, io.size());
        let n_bits = log2_ceil(row_size);
        assert_eq!(row_size, 1 << n_bits);
        assert!(n_bits < Self::Elem::MAX_ROU_PO2);

        let err = unsafe {
            batch_iNTT(
                io.as_device_ptr(),
                n_bits.try_into().unwrap(),
                count.try_into().unwrap(),
            )
        };
        if err.code != 0 {
            panic!("Failure during batch_interpolate_ntt: {err}");
        }
    }

    #[tracing::instrument(skip_all)]
    fn batch_bit_reverse(&self, io: &Self::Buffer<Self::Elem>, count: usize) {
        let row_size = io.size() / count;
        assert_eq!(row_size * count, io.size());
        let bits = log2_ceil(row_size);
        assert_eq!(row_size, 1 << bits);
        let io_size = io.size();

        let kernel = self.module.get_function("multi_bit_reverse").unwrap();
        let params = self.compute_simple_params(io_size);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                io.as_device_ptr(),
                bits,
                io_size
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    #[tracing::instrument(skip_all)]
    fn batch_evaluate_any(
        &self,
        coeffs: &Self::Buffer<Self::Elem>,
        poly_count: usize,
        which: &Self::Buffer<u32>,
        xs: &Self::Buffer<Self::ExtElem>,
        out: &Self::Buffer<Self::ExtElem>,
    ) {
        let po2 = log2_ceil(coeffs.size() / poly_count);
        let count = 1 << po2;
        assert_eq!(poly_count * count, coeffs.size());
        let eval_count = which.size();
        assert_eq!(xs.size(), eval_count);
        assert_eq!(out.size(), eval_count);

        let kernel = self.module.get_function("multi_poly_eval").unwrap();
        let threads_per_block = self.max_threads / 4;
        const BYTES_PER_WORD: u32 = 4;
        const WORDS_PER_FPEXT: u32 = 4;
        let shared_size = threads_per_block * BYTES_PER_WORD * WORDS_PER_FPEXT;
        let (grid, block) = self.compute_simple_params(out.size() * threads_per_block as usize);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<grid, block, shared_size, stream>>>(
                out.as_device_ptr(),
                coeffs.as_device_ptr(),
                which.as_device_ptr(),
                xs.as_device_ptr(),
                count,
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    // #[tracing::instrument(skip_all)]
    fn gather_sample(
        &self,
        dst: &Self::Buffer<Self::Elem>,
        src: &Self::Buffer<Self::Elem>,
        idx: usize,
        size: usize,
        stride: usize,
    ) {
        let kernel = self.module.get_function("gather_sample").unwrap();
        let (grid, block) = self.compute_simple_params(size);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<grid, block, 0, stream>>>(
                dst.as_device_ptr(),
                src.as_device_ptr(),
                idx,
                size,
                stride,
            ))
            .unwrap();
        }
    }

    fn has_unified_memory(&self) -> bool {
        false
    }

    #[tracing::instrument(skip_all)]
    fn zk_shift(&self, io: &Self::Buffer<Self::Elem>, poly_count: usize) {
        let bits = log2_ceil(io.size() / poly_count);
        assert_eq!(io.size(), poly_count * (1 << bits));

        let err = unsafe {
            batch_zk_shift(
                io.as_device_ptr(),
                bits.try_into().unwrap(),
                poly_count.try_into().unwrap(),
            )
        };
        if err.code != 0 {
            panic!("Failure during zk_shift: {err}");
        }
    }

    fn mix_poly_coeffs(
        &self,
        output: &Self::Buffer<Self::ExtElem>,
        mix_start: &Self::ExtElem,
        mix: &Self::ExtElem,
        input: &Self::Buffer<Self::Elem>,
        combos: &Self::Buffer<u32>,
        input_size: usize,
        count: usize,
    ) {
        let mix_start = self.copy_from_extelem("mix_start", &[*mix_start]);
        let mix = self.copy_from_extelem("mix", &[*mix]);

        let kernel = self.module.get_function("mix_poly_coeffs").unwrap();
        let params = self.compute_simple_params(count);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                input.as_device_ptr(),
                combos.as_device_ptr(),
                mix_start.as_device_ptr(),
                mix.as_device_ptr(),
                input_size,
                count
            ))
            .unwrap();
        }
    }

    #[tracing::instrument(skip_all)]
    fn eltwise_add_elem(
        &self,
        output: &Self::Buffer<Self::Elem>,
        input1: &Self::Buffer<Self::Elem>,
        input2: &Self::Buffer<Self::Elem>,
    ) {
        assert_eq!(output.size(), input1.size());
        assert_eq!(output.size(), input2.size());
        let count = output.size();

        let kernel = self.module.get_function("eltwise_add_fp").unwrap();
        let params = self.compute_simple_params(count);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                input1.as_device_ptr(),
                input2.as_device_ptr(),
                count
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    #[tracing::instrument(skip_all)]
    fn eltwise_sum_extelem(
        &self,
        output: &Self::Buffer<Self::Elem>,
        input: &Self::Buffer<Self::ExtElem>,
    ) {
        let count = output.size() / Self::ExtElem::EXT_SIZE;
        let to_add = input.size() / count;
        assert_eq!(output.size(), count * Self::ExtElem::EXT_SIZE);
        assert_eq!(input.size(), count * to_add);

        let kernel = self.module.get_function("eltwise_sum_fpext").unwrap();
        let params = self.compute_simple_params(output.size());
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                input.as_device_ptr(),
                to_add,
                count
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    #[tracing::instrument(skip_all)]
    fn eltwise_copy_elem(
        &self,
        output: &Self::Buffer<Self::Elem>,
        input: &Self::Buffer<Self::Elem>,
    ) {
        let count = output.size();
        assert_eq!(count, input.size());

        let kernel = self.module.get_function("eltwise_copy_fp").unwrap();
        let params = self.compute_simple_params(count);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                input.as_device_ptr(),
                count
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    #[tracing::instrument(skip_all)]
    fn fri_fold(
        &self,
        output: &Self::Buffer<Self::Elem>,
        input: &Self::Buffer<Self::Elem>,
        mix: &Self::ExtElem,
    ) {
        let count = output.size() / Self::ExtElem::EXT_SIZE;
        assert_eq!(output.size(), count * Self::ExtElem::EXT_SIZE);
        assert_eq!(input.size(), output.size() * FRI_FOLD);
        let mix = self.copy_from_extelem("mix", &[*mix]);

        let kernel = self.module.get_function("fri_fold").unwrap();
        let params = self.compute_simple_params(count);
        unsafe {
            let stream = &self.stream;
            launch!(kernel<<<params.0, params.1, 0, stream>>>(
                output.as_device_ptr(),
                input.as_device_ptr(),
                mix.as_device_ptr(),
                count
            ))
            .unwrap();
        }
        self.stream.synchronize().unwrap();
    }

    fn hash_fold(&self, io: &Self::Buffer<Digest>, input_size: usize, output_size: usize) {
        assert_eq!(input_size, 2 * output_size);
        self.hash.as_ref().unwrap().hash_fold(self, io, output_size);
    }

    #[tracing::instrument(skip_all)]
    fn hash_rows(&self, output: &Self::Buffer<Digest>, matrix: &Self::Buffer<Self::Elem>) {
        self.hash.as_ref().unwrap().hash_rows(self, output, matrix);
    }

    fn get_hash_suite(&self) -> &HashSuite<Self::Field> {
        self.hash.as_ref().unwrap().get_hash_suite()
    }

    fn prefix_products(&self, io: &Self::Buffer<Self::ExtElem>) {
        io.view_mut(|io| {
            for i in 1..io.len() {
                io[i] *= io[i - 1];
            }
        });
    }
}

fn div_ceil(a: u32, b: u32) -> u32 {
    (a.checked_add(b).unwrap() - 1) / b
}

pub fn prefix_products(io: &mut UnifiedBuffer<DeviceExtElem>) {
    let len = io.len();
    let io = io.as_mut_slice();
    for i in 1..len {
        io[i].0 *= io[i - 1].0;
    }
}

#[cfg(test)]
mod tests {
    use test_log::test;

    use super::{CudaHalPoseidon, CudaHalPoseidon2, CudaHalSha256};
    use crate::hal::testutil;

    #[test]
    #[should_panic]
    fn check_req() {
        testutil::check_req(CudaHalSha256::new());
    }

    #[test]
    fn eltwise_add_elem() {
        testutil::eltwise_add_elem(CudaHalSha256::new());
    }

    #[test]
    fn eltwise_copy_elem() {
        testutil::eltwise_copy_elem(CudaHalSha256::new());
    }

    #[test]
    fn eltwise_sum_extelem() {
        testutil::eltwise_sum_extelem(CudaHalSha256::new());
    }

    #[test]
    fn hash_rows_sha256() {
        testutil::hash_rows(CudaHalSha256::new());
    }

    #[test]
    fn hash_fold_sha256() {
        testutil::hash_fold(CudaHalSha256::new());
    }

    #[test]
    fn hash_rows_poseidon() {
        testutil::hash_rows(CudaHalPoseidon::new());
    }

    #[test]
    fn hash_fold_poseidon() {
        testutil::hash_fold(CudaHalPoseidon::new());
    }

    #[test]
    fn hash_rows_poseidon2() {
        testutil::hash_rows(CudaHalPoseidon2::new());
    }

    #[test]
    fn hash_fold_poseidon2() {
        testutil::hash_fold(CudaHalPoseidon2::new());
    }

    #[test]
    fn fri_fold() {
        testutil::fri_fold(CudaHalSha256::new());
    }

    #[test]
    fn batch_expand_into_evaluate_ntt() {
        testutil::batch_expand_into_evaluate_ntt(CudaHalSha256::new());
    }

    #[test]
    fn batch_interpolate_ntt() {
        testutil::batch_interpolate_ntt(CudaHalSha256::new());
    }

    #[test]
    fn batch_bit_reverse() {
        testutil::batch_bit_reverse(CudaHalSha256::new());
    }

    #[test]
    fn batch_evaluate_any() {
        testutil::batch_evaluate_any(CudaHalSha256::new());
    }

    #[test]
    fn gather_sample() {
        testutil::gather_sample(CudaHalSha256::new());
    }

    #[test]
    fn zk_shift() {
        testutil::zk_shift(CudaHalSha256::new());
    }

    #[test]
    fn mix_poly_coeffs() {
        testutil::mix_poly_coeffs(CudaHalSha256::new());
    }
}
