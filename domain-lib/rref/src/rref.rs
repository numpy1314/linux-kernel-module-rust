//! RRef (Remote Reference) - 远程引用类型，用于在domain之间安全地共享数据
//!
//! RRef是实现模块隔离的关键组件，它提供了：
//! 1. 跨domain的数据共享
//! 2. 引用计数和生命周期管理
//! 3. 数据所有权的安全转移
//! 4. 类型安全的接口
//!
//! Reference: https://std-dev-guide.rust-lang.org/policy/specialization.html
use alloc::collections::BTreeMap;
use core::{
    alloc::Layout,
    any::TypeId,
    fmt::{Debug, Formatter},
    ops::{Deref, DerefMut},
};

use spin::Mutex;

use super::{CustomDrop, RRefable, SharedData, TypeIdentifiable};

/// RRef<T> - 远程引用类型
/// 
/// 这是实现模块隔离的核心数据结构，包含以下关键特性：
/// 1. 数据存储在共享堆中，带有domain ID标记
/// 2. 支持跨domain的安全数据访问
/// 3. 在热升级时支持数据所有权的转移
/// 
/// 内存布局（repr(C)确保稳定的内存布局）：
/// +-------------------+-------------------+-----------+
/// | domain_id_pointer |   value_pointer   |   exist   |
/// +-------------------+-------------------+-----------+
/// |  指向domain ID    |  指向实际数据     | 存在标志  |
/// +-------------------+-------------------+-----------+
#[repr(C)]
pub struct RRef<T>
where
    T: 'static + RRefable,
{
    /// domain_id_pointer: 指向存储domain ID的内存地址
    /// 这个指针指向共享堆中的一个u64值，标识数据属于哪个domain
    /// 在热升级时，可以通过修改这个值来转移数据所有权
    pub(crate) domain_id_pointer: *mut u64,
    
    /// value_pointer: 指向实际数据的内存地址
    /// 数据存储在共享堆中，所有domain都可以访问
    pub(crate) value_pointer: *mut T,
    
    /// exist: 存在标志，用于防止双重释放
    /// 当数据被转移到其他domain时，设为true以避免在当前domain释放
    pub(crate) exist: bool,
}

// 安全实现标记trait，确保RRef可以在domain间安全传递
unsafe impl<T: RRefable> RRefable for RRef<T> {}
unsafe impl<T: RRefable> Send for RRef<T> where T: Send {}
unsafe impl<T: RRefable> Sync for RRef<T> where T: Sync {}

pub fn drop_no_type<T: CustomDrop>(ptr: *mut u8) {
    let ptr = ptr as *mut T;
    unsafe { &mut *ptr }.custom_drop();
}

type DropFn = fn(ptr: *mut u8);
static DROP: Mutex<BTreeMap<TypeId, DropFn>> = Mutex::new(BTreeMap::new());

pub fn drop_domain_share_data(id: TypeId, ptr: *mut u8) {
    let drop = DROP.lock();
    let drop_fn = drop.get(&id).unwrap();
    drop_fn(ptr);
}

impl<T: RRefable> RRef<T>
where
    T: TypeIdentifiable,
{
    pub(crate) unsafe fn new_with_layout(value: T, layout: Layout, init: bool) -> RRef<T> {
        let type_id = T::type_id();
        let mut drop_guard = DROP.lock();
        drop_guard.entry(type_id).or_insert(drop_no_type::<T>);
        drop(drop_guard);

        let allocation = match crate::share_heap_alloc(layout, type_id, drop_domain_share_data) {
            Some(allocation) => allocation,
            None => panic!("Shared heap allocation failed"),
        };
        let value_pointer = allocation.value_pointer as *mut T;
        *allocation.domain_id_pointer = crate::domain_id();
        if init {
            core::ptr::write(value_pointer, value);
        }
        RRef {
            domain_id_pointer: allocation.domain_id_pointer,
            value_pointer,
            exist: false,
        }
    }

    pub fn new(value: T) -> RRef<T> {
        let layout = Layout::new::<T>();
        unsafe { Self::new_with_layout(value, layout, true) }
    }

    pub fn new_aligned(value: T, align: usize) -> RRef<T> {
        let size = core::mem::size_of::<T>();
        let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
        unsafe { Self::new_with_layout(value, layout, true) }
    }

    pub fn new_uninit() -> RRef<T> {
        let layout = Layout::new::<T>();
        unsafe {
            Self::new_with_layout(
                core::mem::MaybeUninit::uninit().assume_init(),
                layout,
                false,
            )
        }
    }

    pub fn new_uninit_aligned(align: usize) -> RRef<T> {
        let size = core::mem::size_of::<T>();
        let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
        unsafe {
            Self::new_with_layout(
                core::mem::MaybeUninit::uninit().assume_init(),
                layout,
                false,
            )
        }
    }

    pub fn domain_id(&self) -> u64 {
        unsafe { *self.domain_id_pointer }
    }
}

impl<T: RRefable> Deref for RRef<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.value_pointer }
    }
}

impl<T: RRefable> DerefMut for RRef<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.value_pointer }
    }
}

impl<T: RRefable> Drop for RRef<T> {
    fn drop(&mut self) {
        if self.exist {
            return;
        }
        log::warn!("<drop> for RRef {:#x}", self.value_pointer as usize);
        self.custom_drop();
    }
}

impl<T: RRefable> CustomDrop for RRef<T> {
    fn custom_drop(&mut self) {
        if self.exist {
            return;
        }
        log::warn!("<custom_drop> for RRef {:#x}", self.value_pointer as usize);
        let value = unsafe { &mut *self.value_pointer };
        value.custom_drop();
        crate::share_heap_dealloc(self.value_pointer as *mut u8);
    }
}

impl<T: RRefable + Debug> Debug for RRef<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let value = unsafe { &*self.value_pointer };
        let domain_id = unsafe { *self.domain_id_pointer };
        f.debug_struct("RRef")
            .field("value", value)
            .field("domain_id", &domain_id)
            .finish()
    }
}

impl<T: RRefable> SharedData for RRef<T> {
    /// move_to - 将数据所有权转移到新的domain
    /// 
    /// 这是热升级时数据迁移的核心方法：
    /// 1. 读取当前的domain ID（旧domain）
    /// 2. 将domain ID指针更新为新domain的ID
    /// 3. 返回旧的domain ID，用于资源清理
    /// 
    /// 示例：在热升级时，将数据从旧domain迁移到新domain
    /// ```rust
    /// let rref: RRef<Data> = ...;
    /// let old_domain_id = rref.move_to(new_domain_id);
    /// // 现在数据属于new_domain_id，旧domain不应该再访问它
    /// ```
    fn move_to(&self, new_domain_id: u64) -> u64 {
        unsafe {
            // 步骤1: 读取当前的domain ID
            let old_domain_id = *self.domain_id_pointer;
            
            // 步骤2: 原子地更新domain ID指针
            // 注意：这里不是原子操作，但在热升级流程中由锁保护
            *self.domain_id_pointer = new_domain_id;
            
            // 步骤3: 返回旧的domain ID
            old_domain_id
        }
    }
}
