//! 统一 IO 发牌调度层（L1）：所有显式发起的解码/预取类 IO 经这里发牌，
//! 评级探测 worker 只读观察。**调度器只决定"何时允许开工"（admission），
//! 不代理读路径字节**——被拒绝的请求由各服务挂 deferred 队列，完成事件
//! 驱动逐个放行。
//!
//! 纳管范围与介质事实（用户调研定案）：
//! - **目录模式 = 最重**：评级 XMP 头随机读（≤512KB/文件）与大图解码
//!   （18MB 级）在同一介质正面撞（实测 3s 墙钟 vs 1.7s 解码）——Decode
//!   发牌 + rating worker 逐项让路覆盖；
//! - **zip 流式快路径 = 次重**：流式解码/预取直读 NAS 上的压缩包——
//!   Decode/Prefetch 互斥规则覆盖；
//! - **缓存/解压路径无需覆盖**：非流式解码读本地缓存（ImageSource::File）、
//!   解压会话读 NAS 写本地、raw 缓存源同理——读写分离两块盘；
//! - **mpv stream_cb / 解压会话 = 外部租户不纳管**：视频/图片两态互斥
//!   （mpv 回调不会与图片解码并发）、解压会话本身单在途串行——不是
//!   "不能纳管"，是"不需要纳管"。边角：流式 zip 解码与解压会话同时跑
//!   会在 NAS 相撞，Decode 类不区分来源、顺带覆盖。
//!
//! 类规则（优先级占槽模型，槽位统一算术 `total < io_parallel_max_num`）：
//! - `Decode`（最高优先级）：`total < max` 才发牌；超出进 deferred 队列
//!   FIFO（保序 = 单图串行链"当前张 → +1 → −1"自然涌现）；
//! - `Prefetch`：`total < max`，且 `prefetch_yield` 开启时须
//!   `decode_out == 0`，`prefetch_out < 1`（单在途）；超出 deferred；
//! - `Rating`（最低优先级）：`total < max` 且单在途；**派发顺序保证
//!   优先级**——App 侧评级派发闸（deferred 非空让位）+ 跨帧派发顺序，
//!   高优先级请求来了占槽排队，在途评级文件读完让出（文件粒度抢占）。
//!   rating_yield=false 时派发闸门旁路（后台扫语义，槽位照占）
//!
//! 记账口径（fail-open）：`complete` 带饱和下溢保护，看门狗剔除与迟到
//! 消息极端时序下可能对同一在途双记一次完成——计数瞬时偏低只会多放行
//! 一路（fail-open），不会卡死。单 UI 线程发牌 + poll 回执，无并发写者。

use crate::config::IoConfig;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Prefetch 类并行上限（定值）：任一时刻最多 1 路预取在途
pub const PREFETCH_MAX_OUT: usize = 1;

/// io_parallel_max_num 配置钳制域：防 0（死锁）与病态大值（IO 风暴）
const PARALLEL_MIN: usize = 1;
const PARALLEL_MAX: usize = 16;

/// IO 发牌类（优先级 Decode > Prefetch > Rating，槽位统一算术）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoClass {
    /// 显式解码：图片 base/full 高清、raw 帧软解、raw 直方图
    Decode,
    /// 后台预取：±1 邻居、对比候选（user 可见内容未到，空闲预算先行备好）
    Prefetch,
    /// 评级探测：单文件随机头读（≤512KB）。占槽记账（在途时占住一个
    /// 槽位，高优先级请求来了排队等它**读完当前文件**让出——文件粒度
    /// 抢占）；派发顺序保证最低优先级（poll 先放行 deferred 再派评级）
    Rating,
}

/// 预算快照（reload 整体替换，读侧持锁取快照，无撕裂）。rating_yield 不进
/// 预算——评级让路语义在 App 侧派发闸门（deferred 非空不派发），admission
/// 保持纯槽位算术
struct IoBudget {
    decode_max: usize,
    prefetch_yield: bool,
}

/// 统一 IO 发牌调度器（Arc 共享；计数为原子量——UI 线程发牌/回执，
/// rating worker 只读观察）
pub struct IoScheduler {
    decode_out: AtomicUsize,
    prefetch_out: AtomicUsize,
    rating_out: AtomicUsize,
    budget: Mutex<IoBudget>,
}

impl IoScheduler {
    pub fn new(cfg: &IoConfig) -> Arc<Self> {
        Arc::new(Self::build(cfg))
    }

    fn build(cfg: &IoConfig) -> Self {
        Self {
            decode_out: AtomicUsize::new(0),
            prefetch_out: AtomicUsize::new(0),
            rating_out: AtomicUsize::new(0),
            budget: Mutex::new(IoBudget {
                decode_max: cfg.io_parallel_max_num.clamp(PARALLEL_MIN, PARALLEL_MAX),
                prefetch_yield: cfg.prefetch_yield,
            }),
        }
    }

    /// 热重载（顶栏「重载配置」）：整体替换预算。已在途请求不受影响
    /// （超发只在新发牌时收回）
    pub fn reload(&self, cfg: &IoConfig) {
        let mut b = self.budget.lock().unwrap();
        b.decode_max = cfg.io_parallel_max_num.clamp(PARALLEL_MIN, PARALLEL_MAX);
        b.prefetch_yield = cfg.prefetch_yield;
    }

    /// 发牌判定（只读，不记账）：该类此刻是否允许开工。
    /// **优先级占槽模型**：三类共用 `io_parallel_max_num` 个槽位，槽位
    /// 算术统一为 `total < max`（total = 解码+预取+评级在途之和）；类间
    /// 优先级不靠 admission 区分，靠**派发顺序**保证（App 侧 poll 编排：
    /// 评级派发先于解码/预取的 deferred 放行一帧——同帧让位、跨帧保持
    /// 优先级；deferred 放行内部低优先级 Prefetch 可被 Decode/Full 越过）。
    /// admission 是纯槽位算术，数学上排除优先级反转
    pub fn admission(&self, class: IoClass) -> bool {
        let b = self.budget.lock().unwrap();
        let total = self.decode_out.load(Ordering::Relaxed)
            + self.prefetch_out.load(Ordering::Relaxed)
            + self.rating_out.load(Ordering::Relaxed);
        match class {
            IoClass::Decode => total < b.decode_max,
            // 预取让路：解码在途时预取一律推迟（用户可见内容优先占住 IO）
            IoClass::Prefetch => {
                total < b.decode_max
                    && (self.decode_out.load(Ordering::Relaxed) == 0 || !b.prefetch_yield)
                    && self.prefetch_out.load(Ordering::Relaxed) < PREFETCH_MAX_OUT
            }
            // 评级单在途（单 worker 一次读一个文件）；占槽算术与解码同款，
            // 让路语义（deferred 非空不派发）在 App 侧派发闸门按
            // rating_yield 分流
            IoClass::Rating => total < b.decode_max && self.rating_out.load(Ordering::Relaxed) < 1,
        }
    }

    /// 记账开工（admission 通过后由发牌方调用；单 UI 线程，无竞态）
    pub fn issue(&self, class: IoClass) {
        let c = match class {
            IoClass::Decode => &self.decode_out,
            IoClass::Prefetch => &self.prefetch_out,
            IoClass::Rating => &self.rating_out,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// 记账完成（饱和下溢：双记完成只 fail-open 多放行，不卡死）
    pub fn complete(&self, class: IoClass) {
        let c = match class {
            IoClass::Decode => &self.decode_out,
            IoClass::Prefetch => &self.prefetch_out,
            IoClass::Rating => &self.rating_out,
        };
        let _ = c.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// Decode 在途数（日志观测：排队/开工现场打印在途计数）
    pub fn decode_out(&self) -> usize {
        self.decode_out.load(Ordering::Relaxed)
    }

    /// Prefetch 在途数（日志观测：排队/开工现场打印在途计数）
    pub fn prefetch_out(&self) -> usize {
        self.prefetch_out.load(Ordering::Relaxed)
    }

    /// Rating 在途数（日志观测：排队/开工现场打印在途计数）
    pub fn rating_out(&self) -> usize {
        self.rating_out.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max: usize, py: bool, _ry: bool) -> IoConfig {
        IoConfig {
            io_parallel_max_num: max,
            prefetch_yield: py,
            rating_yield: _ry,
        }
    }

    /// admission 矩阵：Decode 预算、Prefetch 让路与单在途
    #[test]
    fn admission_矩阵() {
        let s = IoScheduler::build(&cfg(1, true, true));
        // Decode 预算 1：第 2 路拒绝
        assert!(s.admission(IoClass::Decode));
        s.issue(IoClass::Decode);
        assert!(!s.admission(IoClass::Decode));
        // 预取让路：解码在途 → Prefetch 拒
        assert!(!s.admission(IoClass::Prefetch));
        s.complete(IoClass::Decode);
        // 解码空闲 → Prefetch 过；第 2 路预取拒（单在途）
        assert!(s.admission(IoClass::Prefetch));
        s.issue(IoClass::Prefetch);
        assert!(!s.admission(IoClass::Prefetch));
        s.complete(IoClass::Prefetch);
        // prefetch_yield=false：解码在途也放行预取（不让路解码空闲窗；
        // 槽位算术仍生效——max=1 时解码占满即无空槽，语义只在 max≥2 显现）
        let s2 = IoScheduler::build(&cfg(2, false, false));
        s2.issue(IoClass::Decode);
        assert!(s2.admission(IoClass::Prefetch), "yield 关：借空槽随行");
        assert!(s2.admission(IoClass::Rating), "rating_yield 关：同样借位");
        s2.issue(IoClass::Prefetch);
        assert!(!s2.admission(IoClass::Prefetch), "预取单在途");
        assert!(!s2.admission(IoClass::Rating), "槽位占满（total=max）评级拒");
    }

    /// 串行链涌现：Decode 占满 → Prefetch 拒 → complete → Prefetch 过
    /// （extract/compare 的手工让路守卫在调度器规则下的等价性根基）
    #[test]
    fn 串行链涌现() {
        let s = IoScheduler::new(&cfg(1, true, true));
        s.issue(IoClass::Decode);
        assert!(!s.admission(IoClass::Prefetch));
        s.complete(IoClass::Decode);
        assert!(s.admission(IoClass::Prefetch));
        s.issue(IoClass::Prefetch);
        assert_eq!(s.prefetch_out(), 1);
        s.complete(IoClass::Prefetch);
        assert_eq!(s.prefetch_out(), 0);
        assert_eq!(s.decode_out(), 0);
    }

    /// complete 饱和下溢（fail-open 口径）
    #[test]
    fn complete_饱和下溢() {
        let s = IoScheduler::build(&cfg(1, true, true));
        s.complete(IoClass::Decode); // 0 下溢不动
        s.complete(IoClass::Prefetch);
        assert_eq!(s.decode_out(), 0);
        assert_eq!(s.prefetch_out(), 0);
    }

    /// reload 生效：预算放开后此前被拒的类放行；钳制域拦 0 与超界
    #[test]
    fn reload_生效与钳制() {
        let s = IoScheduler::new(&cfg(1, true, true));
        s.issue(IoClass::Decode);
        assert!(!s.admission(IoClass::Decode));
        s.reload(&cfg(4, false, true));
        assert!(s.admission(IoClass::Decode));
        // 钳制：0 → 1，99 → 16
        s.complete(IoClass::Decode);
        let s2 = IoScheduler::build(&cfg(0, true, true));
        s2.issue(IoClass::Decode);
        assert!(!s2.admission(IoClass::Decode), "0 应钳到 1：1 路在途即拒");
        s2.complete(IoClass::Decode);
        let s3 = IoScheduler::build(&cfg(99, true, true));
        for _ in 0..PARALLEL_MAX {
            assert!(s3.admission(IoClass::Decode));
            s3.issue(IoClass::Decode);
        }
        assert!(!s3.admission(IoClass::Decode), "99 应钳到 16");
    }

    /// Rating admission：纯槽位算术（total<max + 单在途）；让路语义在
    /// App 侧派发闸门（deferred 非空不派发），调度器不感知 rating_yield
    #[test]
    fn rating_admission_让路() {
        let s = IoScheduler::new(&cfg(1, true, true));
        assert!(s.admission(IoClass::Rating));
        s.issue(IoClass::Rating);
        assert!(!s.admission(IoClass::Rating), "评级单在途");
        s.complete(IoClass::Rating);
        s.issue(IoClass::Decode);
        assert!(!s.admission(IoClass::Rating), "解码占槽（max=1）无空位");
        s.complete(IoClass::Decode);
        s.issue(IoClass::Prefetch);
        assert!(!s.admission(IoClass::Rating), "预取占槽同样无空位");
    }

    /// Rating 借位：max≥2 时解码/预取占不满槽位，评级借空槽随行
    /// （App 闸门按 rating_yield 分流让路，槽位算术本身一视同仁）
    #[test]
    fn rating_admission_借位() {
        let s = IoScheduler::new(&cfg(4, true, true));
        s.issue(IoClass::Decode);
        assert!(s.admission(IoClass::Rating), "max=4 解码 1 路评级应借位随行");
        s.issue(IoClass::Rating);
        assert!(!s.admission(IoClass::Rating), "评级仍单在途");
        s.complete(IoClass::Rating);
        // 再补 3 路解码占满（连同首路共 4 = max）
        for _ in 0..3 {
            s.issue(IoClass::Decode);
        }
        assert!(!s.admission(IoClass::Rating), "槽位占满评级拒");
        s.complete(IoClass::Decode);
        assert!(s.admission(IoClass::Rating), "腾出空位评级恢复");
    }
}
