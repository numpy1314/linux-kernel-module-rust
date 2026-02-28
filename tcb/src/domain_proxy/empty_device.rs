use alloc::boxed::Box;
use core::{any::Any, mem::forget, pin::Pin, sync::atomic::AtomicBool};

use corelib::{LinuxError, LinuxResult};
use interface::{empty_device::EmptyDeviceDomain, Basic};
use kernel::{
    init::InPlaceInit,
    sync::{LongLongPerCpu, Mutex, SRcuData},
};
use rref::{RRefVec, SharedData};

use crate::{
    domain_helper::{free_domain_resource, FreeShared},
    domain_loader::loader::DomainLoader,
    domain_proxy::ProxyBuilder,
};

/// EmptyDeviceDomainProxy - 空设备域代理
/// 这是实现热升级的核心组件，负责管理domain的生命周期和原子替换
#[derive(Debug)]
pub struct EmptyDeviceDomainProxy {
    /// domain: 使用SRcuData包装的实际domain实例，支持无锁读取
    /// SRcuData提供安全的读-复制-更新语义，是实现零停机热升级的关键
    domain: SRcuData<Box<dyn EmptyDeviceDomain>>,
    
    /// lock: 用于保护domain替换操作的互斥锁
    /// 在热升级期间，需要获取此锁以确保原子性
    lock: Pin<Box<Mutex<()>>>,
    
    /// domain_loader: domain加载器，管理ELF文件的加载和内存映射
    domain_loader: Pin<Box<Mutex<DomainLoader>>>,
    
    /// flag: 原子布尔标志，指示是否启用锁定路径
    /// 当进行热升级时，将此标志设为true，所有新请求将走锁定路径
    flag: AtomicBool,
    
    /// counter: 每CPU计数器，用于跟踪当前活跃的读操作数量
    /// 这是实现无锁读取和优雅升级的关键机制
    counter: LongLongPerCpu,
}

impl EmptyDeviceDomainProxy {
    /// new - 创建新的EmptyDeviceDomainProxy实例
    /// 
    /// 参数：
    /// - domain: 实际的domain实现，被SRcuData包装以支持无锁读取
    /// - domain_loader: domain加载器，管理ELF文件的加载和内存映射
    /// 
    /// 初始化过程：
    /// 1. 使用SRcuData包装domain，提供RCU语义
    /// 2. 创建互斥锁，用于保护热升级操作
    /// 3. 创建domain_loader的互斥锁
    /// 4. 初始化原子标志为false（正常模式）
    /// 5. 初始化每CPU计数器，用于跟踪活跃读操作
    pub fn new(domain: Box<dyn EmptyDeviceDomain>, domain_loader: DomainLoader) -> Self {
        EmptyDeviceDomainProxy {
            // 使用SRcuData包装domain，这是实现无锁读取的关键
            // SRcuData基于Linux内核的SRCU机制，允许读者在持有引用时睡眠
            domain: SRcuData::new(domain),
            
            // 创建互斥锁，用于保护热升级期间的写操作
            // 这个锁在正常操作时不使用，只在热升级时获取
            lock: Box::pin_init(new_mutex!(())).unwrap(),
            
            // domain_loader也需要保护，防止在热升级过程中被并发修改
            domain_loader: Box::pin_init(new_mutex!(domain_loader)).unwrap(),
            
            // 原子标志，指示是否启用锁定路径
            // false: 正常模式，使用无锁路径
            // true: 升级模式，使用锁定路径
            flag: AtomicBool::new(false),
            
            // 每CPU计数器，用于跟踪当前活跃的读操作数量
            // 这是实现优雅升级的关键：等待所有现有读操作完成
            counter: LongLongPerCpu::new(),
        }
    }
}

impl ProxyBuilder for EmptyDeviceDomainProxy {
    type T = Box<dyn EmptyDeviceDomain>;

    fn build(domain: Self::T, domain_loader: DomainLoader) -> Self {
        Self::new(domain, domain_loader)
    }

    fn build_empty(domain_loader: DomainLoader) -> Self {
        Self::new(Box::new(EmptyDeviceDomainEmptyImpl::new()), domain_loader)
    }
    fn build_empty_no_proxy() -> Self::T {
        Box::new(EmptyDeviceDomainEmptyImpl::new())
    }

    fn init_by_box(&self, _argv: Box<dyn Any + Send + Sync>) -> LinuxResult<()> {
        self.init()
    }
}

impl Basic for EmptyDeviceDomainProxy {
    /// domain_id - 获取domain的ID
    /// 
    /// 这是双重路径机制的关键实现：
    /// 1. 正常模式（flag = false）：使用无锁路径 (_domain_id_no_lock)
    /// 2. 升级模式（flag = true）：使用锁定路径 (_domain_id_with_lock)
    /// 
    /// 双重路径设计的目的：
    /// - 正常模式：高性能，无锁访问
    /// - 升级模式：安全，确保升级期间的一致性
    /// 
    /// 原子读取flag确保模式切换是原子的，不会出现中间状态
    fn domain_id(&self) -> u64 {
        // 原子地读取flag标志
        // Relaxed内存序足够，因为这里只需要原子性，不需要与其他操作同步
        if self.flag.load(core::sync::atomic::Ordering::Relaxed) {
            // 升级模式：走锁定路径
            // 在热升级期间，所有新请求都走这个路径
            self._domain_id_with_lock()
        } else {
            // 正常模式：走无锁路径
            // 这是大多数情况下的路径，提供最佳性能
            self._domain_id_no_lock()
        }
    }
}

impl EmptyDeviceDomain for EmptyDeviceDomainProxy {
    fn init(&self) -> LinuxResult<()> {
        self.domain.read_directly(|domain| domain.init())
    }

    fn read(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        if self.flag.load(core::sync::atomic::Ordering::Relaxed) {
            self._read_with_lock(data)
        } else {
            self._read_no_lock(data)
        }
    }

    fn write(&self, data: &RRefVec<u8>) -> LinuxResult<usize> {
        if self.flag.load(core::sync::atomic::Ordering::Relaxed) {
            self._write_with_lock(data)
        } else {
            self._write_no_lock(data)
        }
    }
}

impl EmptyDeviceDomainProxy {
    /// _domain_id - 内部方法：获取domain ID（基础版本）
    /// 
    /// 直接通过SRcuData读取domain的ID，不涉及任何锁或计数器
    /// 这是其他方法的基础构建块
    fn _domain_id(&self) -> u64 {
        // 使用SRcuData的read_directly方法读取domain ID
        // read_directly不获取SRCU读锁，因为这里只是读取一个简单的整数
        self.domain.read_directly(|domain| domain.domain_id())
    }

    /// _domain_id_no_lock - 无锁路径：获取domain ID
    /// 
    /// 这是正常模式下的路径，特点：
    /// 1. 使用每CPU计数器跟踪活跃读操作
    /// 2. 无锁访问，高性能
    /// 3. 支持并发读取
    /// 
    /// 计数器机制：
    /// - 进入时：计数器+1
    /// - 执行操作：读取domain ID
    /// - 退出时：计数器-1
    /// 
    /// 这个计数器用于热升级时等待所有读操作完成
    fn _domain_id_no_lock(&self) -> u64 {
        // 步骤1: 增加当前CPU的计数器
        // 表示有一个新的读操作开始了
        self.counter.get_with(|counter| {
            *counter += 1;
        });
        
        // 步骤2: 实际读取domain ID
        let r = self._domain_id();
        
        // 步骤3: 减少当前CPU的计数器
        // 表示这个读操作完成了
        self.counter.get_with(|counter| {
            *counter -= 1;
        });
        
        // 步骤4: 返回结果
        r
    }

    /// _domain_id_with_lock - 锁定路径：获取domain ID
    /// 
    /// 这是升级模式下的路径，特点：
    /// 1. 获取互斥锁，确保独占访问
    /// 2. 安全，但性能较低
    /// 3. 用于热升级期间的新请求
    /// 
    /// 锁的作用：
    /// - 防止在热升级过程中出现竞态条件
    /// - 确保升级期间的一致性
    /// - 与replace方法中的写锁配合使用
    fn _domain_id_with_lock(&self) -> u64 {
        // 步骤1: 获取互斥锁
        // 这会阻塞，直到锁可用
        let lock = self.lock.lock();
        
        // 步骤2: 在锁保护下读取domain ID
        let r = self._domain_id();
        
        // 步骤3: 释放锁
        // lock在作用域结束时自动释放，这里显式drop以明确意图
        drop(lock);
        
        // 步骤4: 返回结果
        r
    }

    /// _read - 内部方法：读取数据（基础版本）
    /// 
    /// 这是数据迁移的核心方法，处理以下关键步骤：
    /// 1. 获取当前domain的ID
    /// 2. 将数据所有权迁移到当前domain
    /// 3. 调用实际domain的read方法
    /// 4. 将结果数据所有权迁移回原始domain
    /// 
    /// 数据迁移流程：
    /// 输入数据 -> move_to(当前domain) -> domain.read() -> move_to(原始domain) -> 输出数据
    /// 
    /// 这种设计确保：
    /// 1. 数据在访问期间属于正确的domain
    /// 2. 热升级时数据可以安全迁移
    /// 3. 避免数据竞争和所有权混乱
    fn _read(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        // 使用SRcuData的read_directly方法，在RCU保护下访问domain
        let (res, old_id) = self.domain.read_directly(|domain| {
            // 步骤1: 获取当前domain的ID
            // 这个ID用于数据所有权管理
            let id = domain.domain_id();
            
            // 步骤2: 将数据所有权迁移到当前domain
            // data.move_to(id)返回原始domain ID，用于后续恢复
            let old_id = data.move_to(id);
            
            // 步骤3: 调用实际domain的read方法
            // 此时数据属于当前domain，可以安全访问
            let r = domain.read(data);
            
            // 步骤4: 返回结果和原始domain ID
            (r, old_id)
        });
        
        // 处理结果：将数据所有权迁移回原始domain
        res.map(|r| {
            // 将结果数据的所有权迁移回原始domain
            // 这是为了保持数据所有权的一致性
            r.move_to(old_id);
            r
        })
    }

    /// _write - 内部方法：写入数据（基础版本）
    /// 
    /// 与_read类似，但写入操作不需要迁移数据所有权
    /// 因为写入操作不返回数据，只需要确保数据在正确的domain中
    /// 
    /// 注意：写入操作通过引用访问数据，不需要所有权转移
    /// 数据的所有权在调用者那里管理
    fn _write(&self, data: &RRefVec<u8>) -> LinuxResult<usize> {
        // 直接调用domain的write方法
        // 数据通过引用传递，不需要所有权转移
        self.domain.read_directly(|domain| domain.write(data))
    }

    fn _read_no_lock(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        self.counter.get_with(|counter| {
            *counter += 1;
        });
        let r = self._read(data);
        self.counter.get_with(|counter| {
            *counter -= 1;
        });
        r
    }

    fn _write_no_lock(&self, data: &RRefVec<u8>) -> LinuxResult<usize> {
        self.counter.get_with(|counter| {
            *counter += 1;
        });
        let r = self._write(data);
        self.counter.get_with(|counter| {
            *counter -= 1;
        });
        r
    }

    fn _read_with_lock(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        let lock = self.lock.lock();
        let r = self._read(data);
        drop(lock);
        r
    }

    fn _write_with_lock(&self, data: &RRefVec<u8>) -> LinuxResult<usize> {
        let lock = self.lock.lock();
        let r = self._write(data);
        drop(lock);
        r
    }
}

impl EmptyDeviceDomainProxy {
    /// replace - 执行domain的热升级替换
    /// 这是实现零停机热升级的核心方法，包含以下关键步骤：
    /// 1. 获取写锁，阻止新的写操作
    /// 2. 启用锁定路径，让新请求走锁定路径
    /// 3. 等待所有现有读操作完成
    /// 4. 原子替换domain实例
    /// 5. 清理旧domain资源
    pub fn replace(
        &self,
        new_domain: Box<dyn EmptyDeviceDomain>,  // 新版本的domain实例
        domain_loader: DomainLoader,             // 新domain的加载器
    ) -> LinuxResult<()> {
        println!("EmptyDeviceDomainProxy replace - 开始热升级");
        
        // 步骤1: 获取domain_loader的锁，防止在升级过程中加载器被修改
        let mut loader_guard = self.domain_loader.lock();
        
        // 步骤2: 获取写锁，阻止新的写操作
        // 在启用锁定路径之前获取写锁，确保原子性
        let w_lock = self.lock.lock();
        
        // 记录旧domain的ID，用于后续资源清理
        let old_id = self.domain_id();
        
        // 步骤3: 启用锁定路径
        // 将flag设为true，所有新请求将走锁定路径（_with_lock方法）
        self.flag.store(true, core::sync::atomic::Ordering::Relaxed);

        // 步骤4: 等待所有现有的读操作完成
        // 检查每CPU计数器，确保所有无锁读操作都已完成
        while self.counter.sum() != 0 {
            println!("等待所有读操作完成，当前活跃读操作数: {}", self.counter.sum());
            // 在实际实现中，这里可能会调用yield_now()让出CPU
            // yield_now();
        }

        // 步骤5: 初始化新domain
        let new_domain_id = new_domain.domain_id();
        new_domain.init().unwrap();

        // 步骤6: 原子替换domain实例
        // 使用SRcuData的update_directly方法原子地替换domain
        // 这是热升级的关键步骤，确保替换操作是原子的
        let old_domain = self.domain.update_directly(new_domain);

        // 步骤7: 禁用锁定路径
        // 将flag设回false，新请求可以继续走无锁路径
        self.flag
            .store(false, core::sync::atomic::Ordering::Relaxed);
        
        // 步骤8: 清理旧domain资源
        // 将旧domain从Box中取出，但不立即drop
        let real_domain = Box::into_inner(old_domain);
        
        // 忘记旧domain，由free_domain_resource负责清理
        // 这是为了避免双重释放，因为共享数据可能还在被新domain使用
        forget(real_domain);

        // 步骤9: 释放旧domain的资源，但保留共享数据
        // FreeShared::NotFree(new_domain_id)表示共享数据不释放，因为新domain还在使用
        free_domain_resource(old_id, FreeShared::NotFree(new_domain_id));
        
        // 步骤10: 更新domain_loader
        *loader_guard = domain_loader;
        
        // 步骤11: 释放锁
        drop(w_lock);
        drop(loader_guard);
        
        println!("热升级完成，旧domain ID: {} -> 新domain ID: {}", old_id, new_domain_id);
        Ok(())
    }
}

#[derive(Debug)]
pub struct EmptyDeviceDomainEmptyImpl;

impl EmptyDeviceDomainEmptyImpl {
    pub fn new() -> Self {
        EmptyDeviceDomainEmptyImpl
    }
}

impl Basic for EmptyDeviceDomainEmptyImpl {
    fn domain_id(&self) -> u64 {
        u64::MAX
    }
}

impl EmptyDeviceDomain for EmptyDeviceDomainEmptyImpl {
    fn init(&self) -> LinuxResult<()> {
        Ok(())
    }

    fn read(&self, _data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        Err(LinuxError::ENOSYS)
    }

    fn write(&self, _data: &RRefVec<u8>) -> LinuxResult<usize> {
        Err(LinuxError::ENOSYS)
    }
}
