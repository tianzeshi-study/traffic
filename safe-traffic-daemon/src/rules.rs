use crate::controller::Firewall;
use safe_traffic_common::{
    config::{Action, HookType, Rule},
    utils::{ControlSignal, RunState, SignalController, TrafficStats},
};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::stream::{self, StreamExt, TryStreamExt};
use log::{debug, error, info};
use std::{
    net::IpAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{sync::mpsc, time};

const MAX_WINDOW_BUFFER: usize = 60;
const CONCURRENT_SIZE: usize = 10;

/// 单 IP 的滑动窗口记录
#[derive(Clone, Debug)]
struct Window {
    /// 最近 bytes 的循环缓冲
    buffer: Vec<u64>,
    /// 缓冲当前填充位置
    pos: usize,
    /// 上次更新时间
    last_ts: DateTime<Utc>,
}

/// 规则引擎管理所有 IP 的窗口并执行动作
pub struct RuleEngine {
    rules: Vec<Rule>,
    stats: Arc<DashMap<IpAddr, TrafficStats>>,
    handles: DashMap<IpAddr, Vec<String>>,
    windows: DashMap<IpAddr, Window>,
    signal_controller: SignalController,
}

impl RuleEngine {
    /// 新建实例
    pub fn new(rules: Vec<Rule>, stats: Arc<DashMap<IpAddr, TrafficStats>>) -> Self {
        RuleEngine {
            rules,
            stats,
            handles: DashMap::new(),
            windows: DashMap::new(),
            signal_controller: SignalController::new(),
        }
    }

    /// 获取当前运行状态
    #[allow(dead_code)]
    pub async fn get_state(&self) -> RunState {
        self.signal_controller.get_state().await
    }

    /// 暂停执行
    pub async fn pause(&self) -> Result<(), &'static str> {
        self.signal_controller.pause().await
    }

    /// 恢复执行
    pub async fn resume(&self) -> Result<(), &'static str> {
        self.signal_controller.resume().await
    }

    /// 优雅停止
    pub async fn stop(&self) -> Result<(), &'static str> {
        self.signal_controller.stop().await
    }

    /// 检查所有 IP 并在必要时调用防火墙控制
    pub async fn check_and_apply(&self, fw_origin: Arc<Firewall>) -> anyhow::Result<()> {
        let now = Utc::now();
        // 遍历每个 IP 的最新流量
        let entries: Vec<_> = self
            .stats
            .iter()
            // .filter(|entry| !fw_origin.is_excluded(entry.key()))
            .map(|entry| {
                let bps = match fw_origin.hook {
                    HookType::Input => entry.value().rx_delta,
                    HookType::Output => entry.value().tx_delta,
                };
                // 获取或创建滑动窗口
                let mut win = self.windows.entry(*entry.key()).or_insert_with(|| Window {
                    buffer: vec![0; MAX_WINDOW_BUFFER], // 最多支持 60 秒窗口
                    pos: 0,
                    last_ts: now,
                });

                // 如果超过 1 秒，推进循环缓冲
                if (now - win.last_ts).num_seconds() >= 1 {
                    win.pos = (win.pos + 1) % win.buffer.len();
                    let pos = win.pos;
                    win.buffer[pos] = bps;
                    win.last_ts = now;
                }
                let v = win.value().clone();
                (*entry.key(), v)
            })
            .collect();

        debug!(
            "starting checking rule: stats entries count: {}",
            entries.len()
        );

        // 异步并发处理
        stream::iter(entries)
            .filter(|entry| {
                let fw_origin = &fw_origin;
                let ip = entry.0;
                async move { !fw_origin.is_excluded(&ip).await }
            })
            .map(Ok::<_, anyhow::Error>)
            .try_for_each_concurrent(CONCURRENT_SIZE, |(ip, win)| {
                let fw = Arc::clone(&fw_origin);
                async move {
                    // 对每条规则进行检测
                    for rule in &self.rules {
                        if rule.is_excluded(&ip) {
                            debug!("skipping excluded IP: {}", ip);
                            continue;
                        }

                        let window_size = rule.window_secs as usize;
                        // 计算滑动窗口内总流量
                        let sum: u64 = win
                            .buffer
                            .iter()
                            .cycle()
                            .skip((win.pos + win.buffer.len() - window_size) % win.buffer.len())
                            .take(window_size)
                            .sum();
                        let avg_bps = sum / rule.window_secs;
                        // 超过阈值 => 执行动作
                        debug!("{} average bps: {}", &ip, &avg_bps);
                        if avg_bps > rule.threshold_bps {
                            match rule.action {
                                Action::RateLimit {
                                    kbps,
                                    burst,
                                    seconds,
                                } => {
                                    debug!("intend to limit the speed of {} to {}kbps", ip, kbps);

                                    let rule_id =
                                        fw.clone().limit(ip, kbps, burst, seconds).await?;
                                    self.handles
                                        .entry(ip)
                                        .and_modify(|vec| vec.push(rule_id.clone()))
                                        .or_insert_with(|| vec![rule_id]);
                                }
                                Action::Ban { seconds } => {
                                    debug!(
                                        "intend to ban {} for {} seconds",
                                        ip,
                                        seconds.unwrap_or(0)
                                    );

                                    let rule_id = fw.ban(ip, seconds).await?;
                                    self.handles
                                        .entry(ip)
                                        .and_modify(|vec| vec.push(rule_id.clone()))
                                        .or_insert_with(|| vec![rule_id]);
                                }
                            }
                        }

                        self.clean_expiration_rules(rule, ip, Arc::clone(&fw))
                            .await?;
                    }
                    Ok(())
                }
            })
            .await
    }

    // clean expiration rules
    async fn clean_expiration_rules(
        &self,
        rule: &Rule,
        ip: IpAddr,
        fw: Arc<Firewall>,
    ) -> anyhow::Result<()> {
        if let Some(ids) = self.handles.get(&ip) {
            for id in ids.clone() {
                match rule.action {
                    Action::RateLimit {
                        kbps: _,
                        burst: _,
                        seconds,
                    } => {
                        if let Some(seconds) = seconds {
                            if fw.is_expiration(&id, seconds).await {
                                debug!("intend to remove limit rule {} because of expiration", ip);
                                fw.unblock(&id).await?;
                            }
                        }
                        continue;
                    }
                    Action::Ban { seconds } => {
                        if let Some(seconds) = seconds {
                            if fw.is_expiration(&id, seconds).await {
                                debug!("intend to unban {} because of expiration", ip);
                                fw.unblock(&id).await?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// 启动规则引擎主循环，支持暂停/恢复/停止
    pub async fn start(&self, fw: Arc<Firewall>, check_interval: Duration) -> anyhow::Result<()> {
        info!("RuleEngine starting...");

        // 创建控制信号通道
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlSignal>();
        *self.signal_controller.control_tx.lock().await = Some(control_tx);

        // 重置状态
        self.signal_controller.state.store(true, Ordering::Relaxed);
        self.signal_controller
            .stop_flag
            .store(false, Ordering::Relaxed);

        let mut interval = time::interval(check_interval);

        info!("RuleEngine started successfully");

        loop {
            tokio::select! {
                // 处理控制信号 - 给予更高优先级
                signal = control_rx.recv() => {

                    match signal {
                        Some(ControlSignal::Pause) => {
                            info!("RuleEngine pausing...");
                            self.signal_controller.state.store(false, Ordering::Relaxed);
                        }
                        Some(ControlSignal::Resume) => {
                            info!("RuleEngine resuming...");
                            self.signal_controller.state.store(true, Ordering::Relaxed);
                            // 移除这里的notify_waiters调用，因为我们改用select模式
                        }
                        Some(ControlSignal::Stop) => {
                            info!("stopping RuleEngine...");
                            self.signal_controller.stop_flag.store(true, Ordering::Relaxed);
                            break;
                        }
                        None => {
                            error!("Control channel closed unexpectedly");
                            break;
                        }
                    }
                }

                // 定时器tick - 只在运行状态下处理
                _ = interval.tick(), if self.signal_controller.state.load(Ordering::Relaxed) => {
                    // 检查是否需要停止
                    if self.signal_controller.stop_flag.load(Ordering::Relaxed) {
                        break;
                    }

                    // 执行检查和应用规则
                    match self.check_and_apply(Arc::clone(&fw)).await {
                        Ok(_) => {}
                        Err(e) => error!("check and apply failed: {}", e),
                    }
                }

                // 在暂停状态下等待resume信号
                _ = self.signal_controller.resume_notify.notified(),
                  if !self.signal_controller.state.load(Ordering::Relaxed) => {
                    debug!("Resume notification received, but state will be checked in next loop iteration");
                    // 这个分支主要是为了在暂停状态下保持响应性
                    // 实际的状态变更由control_rx.recv()分支处理
                }
            }
        }

        // 清理资源
        info!("RuleEngine performing cleanup...");
        *self.signal_controller.control_tx.lock().await = None;

        info!("RuleEngine stopped gracefully");
        Ok(())
    }
}
