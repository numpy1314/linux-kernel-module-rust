use alloc::boxed::Box;

use kbind

use crate::{bindings, bindings::CRcuData, pr_warn};

#[derive(Debug)]
pub struct SRcuData<T> {
    crcu_data: CRcuData,
    ssp: *mut srcu_struct,
    _marker: core::marker::PhantomData<T>,
}
unsafe impl<T> Sync for SRcuData<T> {}
unsafe impl<T> Send for SRcuData<T> {}

impl<T> SRcuData<T> {
    /// new - 创建新的SRcuData实例
    /// 
    /// 这是SRcuData的构造函数，执行以下关键步骤：
    /// 1. 将数据分配到堆上，获取原始指针
    /// 2. 创建SRCU结构体并初始化
    /// 3. 构建SRcuData实例
    /// 
    /// 内存管理：
    /// - 数据使用Box分配在堆上，然后转换为原始指针
    /// - SRCU结构体也分配在堆上
    /// - 这些内存在SRcuData被drop时释放
    pub fn new(data: T) -> SRcuData<T> {
        // 步骤1: 将数据分配到堆上，获取原始指针
        // Box::into_raw将Box转换为原始指针，转移所有权给调用者
        let v = Box::into_raw(Box::new(data));
        
        // 步骤2: 创建SRCU结构体
        // SRCU (Sleepable Read-Copy-Update) 是Linux内核的RCU变体
        // 允许读者在持有引用时睡眠
        let ssp = Box::into_raw(Box::new(srcu_struct::default()));
        
        // 步骤3: 初始化SRCU结构体
        // 这是内核函数，设置SRCU的内部状态
        unsafe {
            bindings::init_srcu_struct(ssp);
        }
        
        // 步骤4: 构建SRcuData实例
        SRcuData {
            // CRcuData是内核RCU数据结构，存储数据指针
            crcu_data: CRcuData {
                data_ptr: v as *mut core::ffi::c_void,
            },
            // ssp: SRCU结构体指针，用于管理读者计数
            ssp,
            // PhantomData: 类型标记，确保类型安全
            _marker: core::marker::PhantomData,
        }
    }

    /// read - 在RCU保护下读取数据
    /// 
    /// 这是标准的RCU读取操作，特点：
    /// 1. 获取SRCU读锁，注册当前读者
    /// 2. 在RCU保护下访问数据
    /// 3. 释放SRCU读锁
    /// 
    /// SRCU机制：
    /// - 读者获取锁时递增读者计数
    /// - 写者更新数据后等待所有现有读者完成
    /// - 读者可以睡眠，适合长时间持有引用的场景
    /// 
    /// 内存序保证：
    /// - 确保读者看到一致的数据视图
    /// - 防止编译器重排和CPU乱序执行
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // 步骤1: 获取SRCU读锁
        // __srcu_read_lock返回一个索引，用于后续解锁
        // 这个调用会递增当前CPU的读者计数
        let idx = unsafe { bindings::__srcu_read_lock(self.ssp) };
        
        // 步骤2: 在RCU保护下获取数据指针
        // srcu_dereference确保内存屏障，防止乱序执行
        let ptr = srcu_defererence::<T>(&self.crcu_data, self.ssp);
        
        // 步骤3: 将原始指针转换为引用
        // 这里假设指针有效，因为RCU机制保证在读者持有锁期间数据不会被释放
        let v = unsafe { &*ptr };
        
        // 步骤4: 执行用户提供的函数
        // 用户可以在安全的环境中访问数据
        let r = f(v);
        
        // 步骤5: 释放SRCU读锁
        // 递减读者计数，如果这是最后一个读者，可能会唤醒等待的写者
        unsafe {
            bindings::__srcu_read_unlock(self.ssp, idx);
        }
        
        // 步骤6: 返回结果
        r
    }

    /// read_directly - 直接读取数据（无RCU保护）
    /// 
    /// 与read()不同，这个方法不获取SRCU读锁
    /// 适用场景：
    /// 1. 读取简单的、不会改变的数据（如整数ID）
    /// 2. 调用者已经持有其他同步机制
    /// 3. 性能关键路径，需要避免锁开销
    /// 
    /// 警告：
    /// - 不提供RCU的内存序保证
    /// - 数据可能在读取过程中被并发修改
    /// - 只适用于特定场景
    pub fn read_directly<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // 步骤1: 直接获取数据指针
        // 不获取SRCU锁，因此没有读者计数保护
        let ptr = srcu_defererence::<T>(&self.crcu_data, self.ssp);
        
        // 步骤2: 将原始指针转换为引用
        let v = unsafe { &*ptr };
        
        // 步骤3: 执行用户提供的函数
        f(v)
    }

    /// update_directly - 直接更新数据（不等待现有读者）
    /// 
    /// 这是热升级中使用的关键方法，特点：
    /// 1. 原子地替换数据指针
    /// 2. 不等待现有读者完成
    /// 3. 返回旧数据的Box，由调用者负责释放
    /// 
    /// 在EmptyDeviceDomainProxy.replace()中的使用：
    /// 1. 首先启用锁定路径，阻止新读者
    /// 2. 等待所有无锁读者完成（通过每CPU计数器）
    /// 3. 然后调用update_directly原子替换domain
    /// 
    /// 原子性保证：
    /// - srcu_assign_pointer使用内存屏障确保原子更新
    /// - 新指针对所有后续读者立即可见
    /// - 旧指针仍然被现有读者使用，直到他们释放
    pub fn update_directly(&self, data: T) -> Box<T> {
        // 步骤1: 保存旧数据指针
        // 这个指针可能还在被现有读者使用
        let old_ptr = self.crcu_data.data_ptr;
        
        // 步骤2: 创建新数据并获取指针
        // Box::into_raw转移所有权，避免立即释放
        let new_ptr = Box::into_raw(Box::new(data));
        
        // 步骤3: 原子地更新指针
        // srcu_assign_pointer使用RCU赋值原语，包含内存屏障
        // 确保新指针对所有CPU立即可见
        srcu_assign_pointer(&self.crcu_data, new_ptr);
        
        // 步骤4: 将旧指针转换回Box
        // 调用者负责释放这个Box
        let old_data = unsafe { Box::from_raw(old_ptr as *mut T) };
        
        // 步骤5: 返回旧数据
        old_data
    }

    /// update - 更新数据并等待现有读者完成
    /// 
    /// 这是标准的RCU更新操作，特点：
    /// 1. 原子地替换数据指针
    /// 2. 调用synchronize_srcu等待所有现有读者完成
    /// 3. 返回旧数据的Box，可以安全释放
    /// 
    /// 与update_directly的区别：
    /// - update等待所有读者完成，确保旧数据不再被使用
    /// - update_directly不等待，调用者需要自己管理同步
    /// 
    /// synchronize_srcu的作用：
    /// - 等待所有在更新前开始的读者完成
    /// - 确保旧数据的内存可以安全释放
    /// - 这是RCU的"宽限期"概念
    pub fn update(&self, data: T) -> Box<T> {
        // 步骤1: 保存旧数据指针
        let old_ptr = self.crcu_data.data_ptr;
        
        // 步骤2: 创建新数据并获取指针
        let new_ptr = Box::into_raw(Box::new(data));
        
        // 步骤3: 原子地更新指针
        srcu_assign_pointer(&self.crcu_data, new_ptr);
        
        // 调试信息：更新开始
        pr_warn!("before synchronize_srcu");
        
        // 步骤4: 等待所有现有读者完成
        // synchronize_srcu会阻塞，直到所有在更新前开始的读者释放了锁
        // 这确保了旧数据不再被任何读者使用
        synchronize_srcu(self.ssp);
        
        // 调试信息：更新完成
        pr_warn!("after synchronize_srcu");
        
        // 步骤5: 将旧指针转换回Box
        // 现在可以安全释放，因为没有读者在使用它了
        let old_data = unsafe { Box::from_raw(old_ptr as *mut T) };
        
        // 步骤6: 返回旧数据
        old_data
    }
}

impl<T> Drop for SRcuData<T> {
    fn drop(&mut self) {
        unsafe {
            bindings::cleanup_srcu_struct(self.ssp);
            let _v = Box::from_raw(self.ssp);
        }
    }
}

fn srcu_defererence<T>(crcu_data: &CRcuData, ssp: *const srcu_struct) -> *const T {
    unsafe {
        let ptr = bindings::srcu_dereference(crcu_data, ssp);
        ptr as *const T
    }
}

fn srcu_assign_pointer<T>(crcu_data: &CRcuData, new_ptr: *const T) {
    unsafe { bindings::rust_helper_rcu_assign_pointer(crcu_data, new_ptr as _) }
}

fn synchronize_srcu(ssp: *const srcu_struct) {
    unsafe { bindings::synchronize_srcu(ssp as *mut srcu_struct) }
}
