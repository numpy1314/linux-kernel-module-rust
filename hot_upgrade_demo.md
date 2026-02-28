# Domain热升级流程演示

本文档详细演示domain和domainlib如何实现模块隔离和热升级。

## 1. 架构概述

```
+-------------------+      +-------------------+      +-------------------+
|   用户空间工具     |      |      TCB模块       |      |    Domain代理层    |
|   (用户命令)       |----->|   (核心管理)       |----->|   (EmptyDevice等)  |
+-------------------+      +-------------------+      +-------------------+
                                                              |
                                                              v
                                                    +-------------------+
                                                    |   实际Domain      |
                                                    |   (NullDevice等)  |
                                                    +-------------------+
```

## 2. 热升级完整流程

### 步骤1: 准备新版本的Domain

```rust
// domains/drivers/null/null/src/lib.rs (新版本)
pub struct NullDeviceDomainImplV2;  // 新版本实现

impl EmptyDeviceDomain for NullDeviceDomainImplV2 {
    fn read(&self, mut data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        data.as_mut_slice().fill(2);  // 新行为：填充2而不是1
        Ok(data)
    }
    // ... 其他方法
}
```

### 步骤2: 编译并注册新Domain

```bash
# 编译新版本的domain
cd domains/drivers/null
cargo build --release

# 将ELF文件注册到系统
echo "注册新版本domain到内核..."
```

### 步骤3: 触发热升级

```rust
// 用户空间通过sysctl触发热升级
// tcb/src/channel/command.rs
pub fn handle_update_domain_command(old_name: &str, new_name: &str, ty: DomainTypeRaw) {
    println!("收到热升级命令: {} -> {}", old_name, new_name);
    update_domain(old_name, new_name, ty).unwrap();
}
```

### 步骤4: 执行原子替换（核心流程）

```rust
// tcb/src/domain_proxy/empty_device.rs - replace()方法
pub fn replace(&self, new_domain: Box<dyn EmptyDeviceDomain>, domain_loader: DomainLoader) {
    println!("开始热升级流程...");
    
    // 阶段1: 准备阶段 - 获取锁，阻止新请求
    let mut loader_guard = self.domain_loader.lock();
    let w_lock = self.lock.lock();  // 获取写锁
    let old_id = self.domain_id();
    
    // 阶段2: 切换模式 - 启用锁定路径
    self.flag.store(true, Ordering::Relaxed);  // 新请求走锁定路径
    
    // 阶段3: 等待现有请求完成
    while self.counter.sum() != 0 {  // 检查每CPU计数器
        println!("等待{}个读操作完成...", self.counter.sum());
    }
    
    // 阶段4: 原子替换
    let new_domain_id = new_domain.domain_id();
    new_domain.init().unwrap();  // 初始化新domain
    
    // 关键步骤：原子替换domain实例
    let old_domain = self.domain.update_directly(new_domain);
    
    // 阶段5: 恢复模式
    self.flag.store(false, Ordering::Relaxed);  // 恢复无锁路径
    
    // 阶段6: 清理资源
    let real_domain = Box::into_inner(old_domain);
    forget(real_domain);  // 避免立即释放，由资源管理器处理
    
    // 迁移共享数据
    free_domain_resource(old_id, FreeShared::NotFree(new_domain_id));
    
    // 阶段7: 更新加载器
    *loader_guard = domain_loader;
    
    println!("热升级完成: 旧ID={} -> 新ID={}", old_id, new_domain_id);
}
```

### 步骤5: 数据迁移（RRef系统）

```rust
// 在domain替换过程中，共享数据通过RRef迁移
// domain-lib/rref/src/rref.rs
impl<T: RRefable> SharedData for RRef<T> {
    fn move_to(&self, new_domain_id: u64) -> u64 {
        unsafe {
            let old_domain_id = *self.domain_id_pointer;
            *self.domain_id_pointer = new_domain_id;  // 更新所有权
            old_domain_id
        }
    }
}

// 在代理层的_read方法中调用move_to
fn _read(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
    let (res, old_id) = self.domain.read_directly(|domain| {
        let id = domain.domain_id();
        let old_id = data.move_to(id);  // 迁移数据到当前domain
        let r = domain.read(data);
        (r, old_id)
    });
    res.map(|r| {
        r.move_to(old_id);  // 迁移回原始domain
        r
    })
}
```

## 3. 模块隔离机制

### 3.1 内存隔离

```rust
// domain-lib/loader/src/lib.rs
pub struct DomainLoader<V: DomainVmOps> {
    entry_point: usize,
    data: Arc<Vec<u8>>,          // ELF文件数据
    virt_start: usize,           // 虚拟地址起始
    module_area: Option<Box<dyn DomainArea>>,  // 独立内存区域
    ident: String,               // domain标识
    text_section: Range<usize>,  // 代码段范围
}

// 每个domain有独立的内存映射
fn load_program(&mut self, elf: &ElfFile) -> Result<()> {
    // 为每个LOAD段创建独立的内存映射
    elf.program_iter()
        .filter(|ph| ph.get_type() == Ok(Type::Load))
        .for_each(|ph| {
            let start_vaddr = ph.virtual_addr() as usize + self.virt_start;
            let permission = DomainMappingFlags::from_ph_flags(ph.flags());
            // 设置内存权限：READ/WRITE/EXECUTE
        });
}
```

### 3.2 数据隔离（RRef系统）

```
共享堆布局：
+-----------------------------------+
| Domain ID | 数据 | 类型信息 | ... |
+-----------------------------------+
    ^           ^
    |           |
domain_id   value_pointer
pointer

RRef内存布局：
+-------------------+-------------------+-----------+
| domain_id_pointer |   value_pointer   |   exist   |
+-------------------+-------------------+-----------+
```

### 3.3 通信隔离

```rust
// 通过trait定义清晰的接口边界
pub trait EmptyDeviceDomain: Basic {
    fn init(&self) -> LinuxResult<()>;
    fn read(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>>;
    fn write(&self, data: &RRefVec<u8>) -> LinuxResult<usize>;
}

// 代理层处理跨domain通信
impl EmptyDeviceDomain for EmptyDeviceDomainProxy {
    fn read(&self, data: RRefVec<u8>) -> LinuxResult<RRefVec<u8>> {
        if self.flag.load(Ordering::Relaxed) {
            self._read_with_lock(data)   // 升级期间：锁定路径
        } else {
            self._read_no_lock(data)     // 正常情况：无锁路径
        }
    }
}
```

## 4. 热升级的优势

### 4.1 零停机时间
- 现有请求可以继续完成
- 新请求在升级期间走锁定路径
- 原子替换确保状态一致性

### 4.2 状态保持
- 共享数据通过`move_to()`迁移
- 内存状态保持不变
- 业务连续性得到保障

### 4.3 安全隔离
- 新旧domain完全隔离
- 故障不会传播
- 支持回滚机制

## 5. 实际使用示例

```bash
# 1. 查看当前domain状态
cat /proc/sys/domain/info

# 2. 注册新版本domain
echo "register null_v2.elf" > /proc/sys/domain/command

# 3. 触发热升级
echo "update null null_v2 1" > /proc/sys/domain/command

# 4. 验证升级结果
dmesg | tail -20
```

## 6. 总结

Domain和domainlib通过以下机制实现模块隔离和热升级：

1. **内存隔离**：每个domain运行在独立的虚拟地址空间
2. **数据隔离**：RRef系统管理跨domain数据共享
3. **通信隔离**：清晰的trait接口定义边界
4. **原子升级**：SRcuData和锁机制确保原子性
5. **状态迁移**：`move_to()`方法转移数据所有权

这种架构为内核模块提供了企业级的可靠性和可维护性，特别适合需要高可用性的场景。